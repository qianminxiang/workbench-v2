//! v3.2.0 F5 团队协作功能面：成员与角色落位、负责人分配、进度写权限。
//!
//! 边界（PRD §3.3.1 / §3.2 读法，用户 2026-11-19 拍板）：
//! - **不复活多用户客户端 / 多账号体系**。本模块只做**本地身份表**的落位：
//!   可建成员、可改角色、可停用，成员身份由 `member` 表承载，不引入登录态、
//!   不引入多账号切换。跨端邀请仍与同步形态绑定，随档位决定（本版不做）。
//! - 因此「权限」在本版是**单机态可验证口径**：同一条库里既有负责人也有成员表，
//!   谁能不能改进度，完全由库里的事实判定（负责人字段 + 成员角色），不依赖网络。
//!
//! 三层口径对齐：
//! 1. 第一层（命令路由）：`apps/desktop/src-tauri` 的 `update_item` / `assign_owner`
//!    命令先判一次，拒绝即返回结构化原因，界面立刻给出明确提示；
//! 2. 第二层（落库）：[`crate::pct::update_item`] 在 `begin_write` 事务内**再判一次**，
//!    绕过命令层直接调引擎也拦得住；
//! 3. 判定都落到同一个函数 [`deny_reason`]，两层不会判出两种结果。
//!
//! 一个刻意的取舍：**`member` 表为空时不启用权限拦截**。
//! 单机首次使用、还没登记任何成员时，「负责人」无从比较，强行拦截会让老用户
//! 升级后连自己的项目都改不动。这正是 PRD §3.3.1「成员管理未落地前只做落位」
//! 的语义——权限随成员表落地而生效，不作默认拒绝。

use crate::store::{now_iso_pub, now_ms_pub, Store, StoreError};
use crate::sync::SyncRow;
use rusqlite::{params, Connection};

/// 两级角色（PRD 原文：只保留管理员与成员，不做部门树/角色矩阵）。
pub const ROLE_ADMIN: &str = "admin";
pub const ROLE_MEMBER: &str = "member";

/// 成员列表的一行（PRD §3.3.1 最小信息集：姓名、角色、最近活跃时间）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct MemberRow {
    pub member_id: String,
    pub display_name: String,
    pub role: String,
    pub status: String,
    pub last_active_at: Option<String>,
    pub created_at: String,
}

/// 本机当前操作身份（`sync_state` 里记的名字与角色；未登记则为本机默认名）。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Identity {
    pub member_name: String,
    pub role: String,
    /// 是否已在 `member` 表里找到同名成员（false = 尚未登记进成员表）
    pub known: bool,
}

/// 一条待提醒的负责人变更（「下一次打开时提醒」，PRD §3.3.2）。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct OwnerChange {
    pub id: String,
    pub item_id: String,
    pub item_title: String,
    pub from_owner: Option<String>,
    pub to_owner: Option<String>,
    pub changed_by: String,
    pub changed_at: String,
    /// 对当前身份而言是新负责人还是原负责人（界面文案用）
    pub side: String,
}

fn member_count(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("SELECT COUNT(*) FROM member", [], |r| r.get::<_, i64>(0))?)
}

fn member_of(conn: &Connection, display_name: &str) -> Option<MemberRow> {
    conn.query_row(
        "SELECT member_id, display_name, role, status, last_active_at, created_at
         FROM member WHERE display_name = ?1",
        params![display_name],
        |r| {
            Ok(MemberRow {
                member_id: r.get(0)?,
                display_name: r.get(1)?,
                role: r.get(2)?,
                status: r.get(3)?,
                last_active_at: r.get(4)?,
                created_at: r.get(5)?,
            })
        },
    )
    .ok()
}

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemberRow> {
    Ok(MemberRow {
        member_id: r.get(0)?,
        display_name: r.get(1)?,
        role: r.get(2)?,
        status: r.get(3)?,
        last_active_at: r.get(4)?,
        created_at: r.get(5)?,
    })
}

fn member_by_id(conn: &Connection, member_id: &str) -> Option<MemberRow> {
    conn.query_row(
        "SELECT member_id, display_name, role, status, last_active_at, created_at
         FROM member WHERE member_id = ?1",
        params![member_id],
        row_of,
    )
    .ok()
}

/// 把操作人标识归一成**成员显示名**。
///
/// 负责人字段存的是显示名（PRD 里负责人按姓名展示、按姓名匹配），而调用方
/// 手里可能只有 `member_id`（同步登录写入的那个 id）。两个口径不统一时，
/// 「负责人本人」会被判成陌生人——权限闸直接把人挡在自己的活外面。
/// 所以判定前统一在这一处换算：先按显示名找，找不到再按 id 找，都没有原样返回
/// （这样未登记的 `local` 之类标识仍能走到「身份未登记」那条分支）。
fn resolve_actor(conn: &Connection, actor: &str) -> String {
    if let Some(m) = member_of(conn, actor) {
        return m.display_name;
    }
    match member_by_id(conn, actor) {
        Some(m) => m.display_name,
        None => actor.to_string(),
    }
}

/// 成员列表（按角色倒序：管理员在前；同级按登记时间）。同时读取本机身份，
/// 未登记时把本机身份补进成员表并设为管理员（PRD：「首位初始化用户默认为管理员」）。
pub fn list_members(store: &Store) -> Result<(Vec<MemberRow>, Identity), StoreError> {
    let conn = store.conn();
    let ident_name = identity_name(conn);
    let ident = ensure_local_admin(store, &ident_name)?;

    let mut st = conn.prepare(
        "SELECT member_id, display_name, role, status, last_active_at, created_at
         FROM member ORDER BY CASE role WHEN 'admin' THEN 0 ELSE 1 END, created_at ASC, member_id ASC",
    )?;
    let rows = st.query_map([], row_of)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok((out, ident))
}

/// 本机身份名：优先 `sync_state.member_name`，其次 `member_id`，最后「本机」。
fn identity_name(conn: &Connection) -> String {
    crate::sync_client::state_get(conn, crate::sync_client::keys::MEMBER_NAME)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| crate::sync_client::state_get(conn, crate::sync_client::keys::MEMBER_ID))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "本机".to_string())
}

/// 本机当前身份（含角色）。**不新增任何账号概念**：只是在成员表里认领同名那一行。
pub fn identity(conn: &Connection) -> Identity {
    let name = identity_name(conn);
    match member_of(conn, &name) {
        Some(m) => Identity {
            member_name: name,
            role: m.role,
            known: true,
        },
        None => Identity {
            member_name: name,
            role: ROLE_MEMBER.to_string(),
            known: false,
        },
    }
}

fn next_id(prefix: &str) -> String {
    // 与既有 id 风格一致：前缀 + 毫秒时间戳（同一毫秒内加序号，避免主键撞车）
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("{}-{}-{}", prefix, now_ms_pub(), n)
}

/// 第一位成员默认管理员：成员表为空时把本机身份写进去。
/// 已有成员时只在**同名缺失**且角色无从判定时补登为管理员——否则会把
/// 「本机身份还没登记」的人永久锁在门外，而他又确实是这台机器的使用者。
fn ensure_local_admin(store: &Store, name: &str) -> Result<Identity, StoreError> {
    let conn = store.conn();
    let n = member_count(conn)?;
    if n > 0 {
        if let Some(m) = member_of(conn, name) {
            return Ok(Identity { member_name: name.into(), role: m.role, known: true });
        }
        // 成员表已有内容、本机身份不在其中：如实标 unknown，**不擅自赋权**。
        // 判定层会据此拒绝写操作并给出「请先登记自己」的提示。
        return Ok(Identity { member_name: name.into(), role: ROLE_MEMBER.into(), known: false });
    }
    let guard = store.begin_write("member-seed")?;
    let c = guard.conn();
    let ts = now_iso_pub();
    c.execute(
        "INSERT INTO member (member_id, display_name, role, created_at, status, last_active_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?4)",
        params![next_id("m"), name, ROLE_ADMIN, ts],
    )?;
    Ok(Identity { member_name: name.into(), role: ROLE_ADMIN.into(), known: true })
}

/// 新建成员（PRD §3.3.1「可建」）。仅管理员可建；重名直接拒绝，
/// 因为负责人是用显示名匹配的，重名会让权限判定失去唯一性。
pub fn create_member(
    store: &Store,
    display_name: &str,
    role: &str,
    by: &str,
) -> Result<MemberRow, StoreError> {
    let name = display_name.trim();
    if name.is_empty() {
        return Err(StoreError::MigrationFailed {
            script: "member-create".into(),
            error: "成员姓名不能为空".into(),
        });
    }
    if role != ROLE_ADMIN && role != ROLE_MEMBER {
        return Err(StoreError::MigrationFailed {
            script: "member-create".into(),
            error: format!("角色只能是 admin / member，收到「{role}」"),
        });
    }
    let guard = store.begin_write("member-create")?;
    let conn = guard.conn();
    require_admin(conn, by, "新建成员")?;
    if member_of(conn, name).is_some() {
        return Err(StoreError::MigrationFailed {
            script: "member-create".into(),
            error: format!("成员「{name}」已存在（负责人按姓名匹配，不允许重名）"),
        });
    }
    let ts = now_iso_pub();
    let id = next_id("m");
    conn.execute(
        "INSERT INTO member (member_id, display_name, role, created_at, status, last_active_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?4)",
        params![id, name, role, ts],
    )?;
    Ok(MemberRow {
        member_id: id,
        display_name: name.into(),
        role: role.into(),
        status: "active".into(),
        last_active_at: Some(ts.clone()),
        created_at: ts,
    })
}

/// 改角色（PRD §3.3.1「可改角色」）。**最后一名管理员不能降级**——
/// 降了之后库里没有任何人能改角色/成员，等于把团队锁死。
pub fn set_member_role(
    store: &Store,
    member_id: &str,
    role: &str,
    by: &str,
) -> Result<MemberRow, StoreError> {
    if role != ROLE_ADMIN && role != ROLE_MEMBER {
        return Err(StoreError::MigrationFailed {
            script: "member-role".into(),
            error: format!("角色只能是 admin / member，收到「{role}」"),
        });
    }
    let guard = store.begin_write("member-role")?;
    let conn = guard.conn();
    require_admin(conn, by, "变更角色")?;
    let cur = conn
        .query_row(
            "SELECT member_id, display_name, role, status, last_active_at, created_at
             FROM member WHERE member_id = ?1",
            params![member_id],
            |r| Ok(row_of(r)),
        )
        .map_err(|_| StoreError::MigrationFailed {
            script: "member-role".into(),
            error: format!("成员 {member_id} 不存在"),
        })??;
    if cur.status != "active" {
        return Err(StoreError::MigrationFailed {
            script: "member-role".into(),
            error: format!("成员「{}」已停用，先恢复再改角色", cur.display_name),
        });
    }
    if cur.role == ROLE_ADMIN && role == ROLE_MEMBER {
        let admins: i64 = conn.query_row(
            "SELECT COUNT(*) FROM member WHERE role = 'admin' AND status = 'active'",
            [],
            |r| r.get(0),
        )?;
        if admins <= 1 {
            return Err(StoreError::MigrationFailed {
                script: "member-role".into(),
                error: "这是最后一名管理员，降级后无人可改角色与成员".into(),
            });
        }
    }
    conn.execute("UPDATE member SET role = ?1 WHERE member_id = ?2", params![role, member_id])?;
    Ok(MemberRow { role: role.into(), ..cur })
}

/// 停用 / 恢复成员（PRD §3.3.1「可停用」）。停用不做删除——数据与历史都留着，
/// 只是不再允许以该身份写。最后一名启用中的管理员不能停用，理由同角色降级。
pub fn set_member_status(
    store: &Store,
    member_id: &str,
    status: &str,
    by: &str,
) -> Result<MemberRow, StoreError> {
    if status != "active" && status != "disabled" {
        return Err(StoreError::MigrationFailed {
            script: "member-status".into(),
            error: format!("状态只能是 active / disabled，收到「{status}」"),
        });
    }
    let guard = store.begin_write("member-status")?;
    let conn = guard.conn();
    require_admin(conn, by, "停用/恢复成员")?;
    let cur = conn
        .query_row(
            "SELECT member_id, display_name, role, status, last_active_at, created_at
             FROM member WHERE member_id = ?1",
            params![member_id],
            |r| Ok(row_of(r)),
        )
        .map_err(|_| StoreError::MigrationFailed {
            script: "member-status".into(),
            error: format!("成员 {member_id} 不存在"),
        })??;
    if status == "disabled" && cur.role == ROLE_ADMIN {
        let admins: i64 = conn.query_row(
            "SELECT COUNT(*) FROM member WHERE role = 'admin' AND status = 'active'",
            [],
            |r| r.get(0),
        )?;
        if admins <= 1 {
            return Err(StoreError::MigrationFailed {
                script: "member-status".into(),
                error: "这是最后一名启用中的管理员，停用后无人可管理成员".into(),
            });
        }
    }
    conn.execute("UPDATE member SET status = ?1 WHERE member_id = ?2", params![status, member_id])?;
    Ok(MemberRow { status: status.into(), ..cur })
}

/// 成员管理动作的统一前置：只有管理员能做。
fn require_admin(conn: &Connection, by: &str, action: &str) -> Result<(), StoreError> {
    let ident = identity(conn);
    if !ident.known {
        return Err(StoreError::MigrationFailed {
            script: "member-admin-gate".into(),
            error: format!(
                "{action}需要管理员权限：当前身份「{}」尚未登记进成员表",
                ident.member_name
            ),
        });
    }
    if ident.role != ROLE_ADMIN {
        return Err(StoreError::MigrationFailed {
            script: "member-admin-gate".into(),
            error: format!("{action}仅限管理员，「{}」当前是成员", ident.member_name),
        });
    }
    let _ = by; // 调用方已把同一个身份传进来；判定只认库里的角色，不认调用方自称
    Ok(())
}

/// 打一次活跃时间（成员列表的「最近活跃时间」，PRD 明确不做在线状态实时呈现）。
pub fn touch_active(conn: &Connection, name: &str) {
    let ts = now_iso_pub();
    let _ = conn.execute(
        "UPDATE member SET last_active_at = ?1 WHERE display_name = ?2",
        params![ts, name],
    );
}

/// 进度写权限的**唯一判定点**：两层校验都调它，避免两处判出两种结果。
///
/// 返回 `Some(原因)` = 拒绝并原样把原因带进界面；`None` = 允许。
/// 判定顺序即 PRD 口径：① 成员表为空 → 不启用权限（单机老库）；
/// ② 未指定负责人 → 允许（还没分配，不是"谁都能改已分配的"）；
/// ③ 身份即负责人 → 允许；④ 身份是管理员 → 允许；⑤ 其余 → 拒绝并说明当前负责人。
pub fn deny_reason(conn: &Connection, item_id: &str, actor: &str) -> Option<String> {
    let n = member_count(conn).ok()?;
    if n == 0 {
        return None;
    }
    let owner: Option<String> = conn
        .query_row(
            "SELECT owner FROM work_item WHERE id = ?1 AND deleted_at IS NULL",
            params![item_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()?;
    let owner = owner.and_then(|o| if o.trim().is_empty() { None } else { Some(o) });
    let owner = match owner {
        Some(o) => o,
        None => return None, // 未分配：谁都可以先推进，直到有人被指定
    };
    // 操作人可能传进来的是 member_id，先归一到显示名：负责人字段按显示名存，
    // 不归一的话「负责人本人」会被当成陌生人拒掉。
    let actor = resolve_actor(conn, actor);
    // 停用判定必须先于「是否负责人」——否则停用的人只要还挂着负责人就照样能写，
    // 停用在成员管理里就形同虚设。
    if let Some(m) = member_of(conn, &actor) {
        if m.status == "disabled" {
            return Some(format!(
                "「{actor}」已停用，不能改进度（当前负责人：{owner}）"
            ));
        }
    }
    if owner == actor {
        return None;
    }
    match member_of(conn, &actor) {
        Some(m) if m.role == ROLE_ADMIN => None,
        Some(_) => Some(format!(
            "进度修改仅限负责人或管理员：这条的负责人是「{owner}」，你是「{actor}」（成员）"
        )),
        None => Some(format!(
            "无法确认「{actor}」的角色：该身份尚未登记进成员表，请先在「成员」页登记（当前负责人：{owner}）"
        )),
    }
}

/// 负责人分配 / 变更（PRD §3.3.2：负责人单选，变更记入变更历史）。
///
/// 谁能改：管理员，或该条的**当前负责人**（自己把自己的活转出去）。未分配时
/// 任何人都可以先指定——这保证「默认创建人」没落上的历史数据还能被补齐。
/// `to_owner` 传空串 = 取消分配。变更行写 `owner_change_log`，供下次打开时提醒。
pub fn set_item_owner(
    store: &Store,
    item_id: &str,
    to_owner: Option<&str>,
    by: &str,
) -> Result<Option<OwnerChange>, StoreError> {
    let guard = store.begin_write("assign-owner")?;
    let conn = guard.conn();
    let (title, cur_owner): (String, Option<String>) = conn
        .query_row(
            "SELECT title, owner FROM work_item WHERE id = ?1 AND deleted_at IS NULL",
            params![item_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| StoreError::MigrationFailed {
            script: "assign-owner".into(),
            error: format!("工作项 {item_id} 不存在"),
        })?;
    let cur = cur_owner.filter(|o| !o.trim().is_empty());
    let next = to_owner.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    if let Some(ref o) = next {
        // 只允许指派给已登记的启用成员：否则负责人就是个没人认领的字符串，
        // 「负责人视角能改进度」这条验收根本无从演示。
        match member_of(conn, o) {
            Some(m) if m.status == "active" => {}
            Some(_) => {
                return Err(StoreError::MigrationFailed {
                    script: "assign-owner".into(),
                    error: format!("「{o}」已停用，不能指派"),
                })
            }
            None => {
                return Err(StoreError::MigrationFailed {
                    script: "assign-owner".into(),
                    error: format!("「{o}」不在成员表里，请先在「成员」页登记"),
                })
            }
        }
    }

    // 分配权限：管理员 / 当前负责人。判定用**操作人 `by`**，不是本机登录身份——
    // 机器身份取的是本机当前登记人，和这次操作是谁发起的两回事，用它判会把人全放过去。
    // `by` 与负责人字段同为显示名口径，先归一再比（调用方可能只有 member_id）。
    if cur.is_some() && member_count(conn)? > 0 {
        let actor = resolve_actor(conn, by);
        let is_owner = cur.as_deref() == Some(actor.as_str());
        let actor_row = member_of(conn, &actor);
        if let Some(ref m) = actor_row {
            if m.status == "disabled" {
                return Err(StoreError::MigrationFailed {
                    script: "assign-owner".into(),
                    error: format!("「{actor}」已停用，不能改负责人"),
                });
            }
        }
        let is_admin = matches!(actor_row, Some(ref m) if m.role == ROLE_ADMIN);
        if !is_admin && !is_owner {
            return Err(StoreError::MigrationFailed {
                script: "assign-owner".into(),
                error: format!(
                    "只有管理员或当前负责人能改负责人：这条的负责人是「{}」，你是「{actor}」",
                    cur.clone().unwrap_or_default()
                ),
            });
        }
    }

    if cur == next {
        return Ok(None); // 没变：不写历史、不入队，避免空变更把提醒区刷满
    }

    let ts = now_iso_pub();
    let log_id = next_id("ocl");
    conn.execute(
        "INSERT INTO owner_change_log (id, item_id, from_owner, to_owner, changed_by, changed_at, notified_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![log_id, item_id, cur, next, by, ts, None::<String>],
    )?;
    conn.execute(
        "UPDATE work_item SET owner = ?1, updated_at = ?2, updated_by = ?3 WHERE id = ?4",
        params![next, ts, by, item_id],
    )?;

    // 入同步队列：负责人是全员的可见事实，不能只留在本机（PRD「全员可见负责人」）。
    // 失败不影响本地已落库的结果，只是留在队列里等下次补推。
    let mut payload = serde_json::Map::new();
    payload.insert("id".into(), serde_json::json!(item_id));
    payload.insert("owner".into(), serde_json::json!(next));
    payload.insert("updated_at".into(), serde_json::json!(ts));
    payload.insert("updated_by".into(), serde_json::json!(by));
    let row = SyncRow {
        table: "work_item".into(),
        id: item_id.to_string(),
        payload,
        updated_at: now_ms_pub(),
        updated_by: by.to_string(),
        device_id: crate::sync_client::state_get(conn, crate::sync_client::keys::DEVICE_ID)
            .unwrap_or_else(|| "unknown".into()),
        seq: 0,
        deleted_at: None,
    };
    let _ = crate::sync_client::enqueue(conn, &row, &ts);

    Ok(Some(OwnerChange {
        id: log_id,
        item_id: item_id.to_string(),
        item_title: title,
        from_owner: cur,
        to_owner: next,
        changed_by: by.to_string(),
        changed_at: ts,
        side: "to".into(),
    }))
}

/// 新建工作项时的初始化：负责人默认创建人（PRD「负责人单选、默认为创建人」）。
/// 这里**不写变更历史**——初次指定不是「变更」，否则每建一条都往提醒区塞一条。
pub fn init_owner_on_create(conn: &Connection, item_id: &str, creator: &str) -> Result<(), StoreError> {
    if member_count(conn)? == 0 {
        return Ok(()); // 单机未登记成员：保持 owner 为空，等成员表落地后再补
    }
    if member_of(conn, creator).is_none() {
        return Ok(()); // 创建人不在成员表：不写一个没人认领的负责人
    }
    conn.execute(
        "UPDATE work_item SET owner = ?1 WHERE id = ?2 AND (owner IS NULL OR owner = '')",
        params![creator, item_id],
    )?;
    Ok(())
}

/// 待提醒的负责人变更（`notified_at IS NULL` 的行）。对当前身份只取与它相关的：
/// 新负责人（你被派了活）与原负责人（你的活被转走）两种，文案不同。
pub fn pending_owner_changes(conn: &Connection) -> Result<Vec<OwnerChange>, StoreError> {
    let ident = identity_name(conn);
    let mut st = conn.prepare(
        "SELECT l.id, l.item_id, COALESCE(w.title, ''), l.from_owner, l.to_owner,
                l.changed_by, l.changed_at
         FROM owner_change_log l LEFT JOIN work_item w ON w.id = l.item_id
         WHERE l.notified_at IS NULL
         ORDER BY l.changed_at DESC, l.id DESC",
    )?;
    let rows = st.query_map([], |r| {
        Ok(OwnerChange {
            id: r.get(0)?,
            item_id: r.get(1)?,
            item_title: r.get(2)?,
            from_owner: r.get(3)?,
            to_owner: r.get(4)?,
            changed_by: r.get(5)?,
            changed_at: r.get(6)?,
            side: "to".into(),
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        let mut c = r?;
        if c.to_owner.as_deref() == Some(ident.as_str()) {
            c.side = "to".into();
        } else if c.from_owner.as_deref() == Some(ident.as_str()) {
            c.side = "from".into();
        } else {
            continue; // 与当前身份无关：不进提醒区（它不是「相关成员」）
        }
        out.push(c);
    }
    Ok(out)
}

/// 标记已提醒（下一次打开不再重复提醒）。只标传进来的那几条。
pub fn mark_owner_changes_notified(conn: &Connection, ids: &[String]) -> Result<(), StoreError> {
    let ts = now_iso_pub();
    for id in ids {
        conn.execute(
            "UPDATE owner_change_log SET notified_at = ?1 WHERE id = ?2 AND notified_at IS NULL",
            params![ts, id],
        )?;
    }
    Ok(())
}

/// 落库前的 `notified_at`：新建变更默认未提醒（`NULL`，由 SQL 直接传 `None` 落库）。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    /// 每个用例一套独立的临时库（`Store` 只有磁盘库构造，没有内存库：
    /// 迁移层要建归档库与备份目录，纯内存连接跑不通）。
    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        (d, s)
    }

    /// 把本机身份定为「张三」。**必须在 `setup_admin` 之前调用**：
    /// `list_members` 只把「当时的身份」补登为管理员，先把身份定好，后面补登出来的
    /// 管理员才是张三；否则会补登出一个「本机」，而张三在成员表里查无此人 ——
    /// 连自己建的项目都改不动，测出来的失败跟被测逻辑无关。
    fn as_zhangsan(store: &Store) {
        crate::sync_client::state_set(store.conn(), crate::sync_client::keys::MEMBER_NAME, "张三")
            .unwrap();
    }

    /// 建库 → 身份定为张三 → 补登为管理员（PRD「首位初始化用户默认为管理员」）。
    fn setup_admin(store: &Store) {
        as_zhangsan(store);
        list_members(store).unwrap();
    }

    fn seed_item(store: &Store, id: &str, owner: Option<&str>) {
        let c = store.conn();
        // 表上 planned_start_precision / planned_end_precision 都是 NOT NULL
        // （精度必须显式给出，不许"猜"），插入时两列都必须带上。
        c.execute(
            "INSERT INTO work_item (id, fiscal_year, seq_no, board_code, title, progress_pct, status,
                planned_start_precision, planned_end_precision,
                updated_at, updated_by)
             VALUES (?1, 2027, 1, 'policy', '测试项', 0, '待启动',
                'day', 'day', '2026-11-19T00:00:00Z', '张三')",
            params![id],
        )
        .unwrap();
        if let Some(o) = owner {
            c.execute("UPDATE work_item SET owner = ?1 WHERE id = ?2", params![o, id]).unwrap();
        }
    }

    #[test]
    fn 首位身份自动成为管理员且可重复读取() {
        let (_d1, s) = store();
        let (list, ident) = list_members(&s).unwrap();
        assert_eq!(list.len(), 1, "空成员表应补登本机身份");
        assert_eq!(ident.role, ROLE_ADMIN);
        assert!(ident.known);
        let (list2, _) = list_members(&s).unwrap();
        assert_eq!(list2.len(), 1, "重复读取不得再插一条");
    }

    #[test]
    fn 可建成员_可改角色_可停用() {
        let (_d2, s) = store();
        setup_admin(&s);
        let m = create_member(&s, "李四", ROLE_MEMBER, "本机").unwrap();
        assert_eq!(m.role, ROLE_MEMBER);
        // 重名被拒（负责人按姓名匹配）
        assert!(create_member(&s, "李四", ROLE_MEMBER, "本机").is_err());
        let up = set_member_role(&s, &m.member_id, ROLE_ADMIN, "本机").unwrap();
        assert_eq!(up.role, ROLE_ADMIN);
        let dis = set_member_status(&s, &m.member_id, "disabled", "本机").unwrap();
        assert_eq!(dis.status, "disabled");
        // 最后一名启用中的管理员不能停用：把李四恢复后降回成员，只剩本机一个管理员
        set_member_status(&s, &m.member_id, "active", "本机").unwrap();
        set_member_role(&s, &m.member_id, ROLE_MEMBER, "本机").unwrap();
        let ident_rows: Vec<MemberRow> = {
            let mut st = s.conn().prepare("SELECT member_id, display_name, role, status, last_active_at, created_at FROM member WHERE role='admin'").unwrap();
            st.query_map([], row_of).unwrap().map(|r| r.unwrap()).collect()
        };
        assert_eq!(ident_rows.len(), 1);
        assert!(
            set_member_status(&s, &ident_rows[0].member_id, "disabled", "本机").is_err(),
            "最后一名管理员不得停用"
        );
    }

    #[test]
    fn 非管理员不能改成员() {
        let (_d3, s) = store();
        setup_admin(&s);
        let m = create_member(&s, "王五", ROLE_MEMBER, "张三").unwrap();
        assert_eq!(m.display_name, "王五");
        // 把本机身份切成王五：成员表非空且王五存在 → 角色=member
        crate::sync_client::state_set(s.conn(), crate::sync_client::keys::MEMBER_NAME, "王五").unwrap();
        assert!(create_member(&s, "赵六", ROLE_MEMBER, "王五").is_err());
        assert_eq!(identity(s.conn()).role, ROLE_MEMBER);
    }

    #[test]
    fn 成员表为空时不启用进度权限() {
        let (_d4, s) = store();
        as_zhangsan(&s); // 先定身份，再建项；成员表此时为空
        seed_item(&s, "i1", Some("李四"));
        assert_eq!(member_count(s.conn()).unwrap(), 0, "本用例要求成员表为空");
        assert_eq!(deny_reason(s.conn(), "i1", "张三"), None, "成员表为空时不启用权限");
    }

    #[test]
    fn 非负责人非管理员被拒且原因含负责人姓名() {
        let (_d5, s) = store();
        setup_admin(&s);
        let m = create_member(&s, "李四", ROLE_MEMBER, "张三").unwrap();
        seed_item(&s, "i1", Some("李四"));
        assert_eq!(deny_reason(s.conn(), "i1", "李四"), None, "负责人可改");
        // 换成一名普通成员身份：拒绝
        let m2 = create_member(&s, "王五", ROLE_MEMBER, "张三").unwrap();
        let reason = deny_reason(s.conn(), "i1", "王五").expect("应拒绝");
        assert!(reason.contains("李四"), "原因须点名当前负责人：{reason}");
        assert_eq!(deny_reason(s.conn(), "i1", "张三"), None, "管理员可改");
        assert!(m.member_id != m2.member_id);
    }

    #[test]
    fn 未分配负责人时任何人可推进() {
        let (_d6, s) = store();
        setup_admin(&s);
        create_member(&s, "李四", ROLE_MEMBER, "张三").unwrap();
        seed_item(&s, "i1", None);
        assert_eq!(deny_reason(s.conn(), "i1", "李四"), None);
    }

    #[test]
    fn 停用成员被拒且原因说明已停用() {
        let (_d7, s) = store();
        setup_admin(&s);
        let m = create_member(&s, "李四", ROLE_MEMBER, "张三").unwrap();
        seed_item(&s, "i1", Some("李四"));
        set_member_status(&s, &m.member_id, "disabled", "张三").unwrap();
        let reason = deny_reason(s.conn(), "i1", "李四").expect("应拒绝");
        assert!(reason.contains("停用"), "{reason}");
    }

    #[test]
    fn 负责人变更写入历史并生成待提醒() {
        let (_d8, s) = store();
        setup_admin(&s);
        create_member(&s, "李四", ROLE_MEMBER, "张三").unwrap();
        seed_item(&s, "i1", Some("张三"));
        let ch = set_item_owner(&s, "i1", Some("李四"), "张三").unwrap().expect("应产生变更");
        assert_eq!(ch.from_owner.as_deref(), Some("张三"));
        assert_eq!(ch.to_owner.as_deref(), Some("李四"));
        // 当前身份是张三 → 它是「原负责人」那侧
        let pend = pending_owner_changes(s.conn()).unwrap();
        assert_eq!(pend.len(), 1);
        assert_eq!(pend[0].side, "from");
        // 换成李四视角 → 「新负责人」
        crate::sync_client::state_set(s.conn(), crate::sync_client::keys::MEMBER_NAME, "李四").unwrap();
        let pend2 = pending_owner_changes(s.conn()).unwrap();
        assert_eq!(pend2[0].side, "to");
        assert_eq!(pend2[0].item_title, "测试项");
        mark_owner_changes_notified(s.conn(), &[pend2[0].id.clone()]).unwrap();
        assert!(pending_owner_changes(s.conn()).unwrap().is_empty(), "标过就不再提醒");
    }

    #[test]
    fn 负责人未变不写历史不刷提醒() {
        let (_d9, s) = store();
        setup_admin(&s);
        seed_item(&s, "i1", Some("张三"));
        assert!(set_item_owner(&s, "i1", Some("张三"), "张三").unwrap().is_none());
        assert!(pending_owner_changes(s.conn()).unwrap().is_empty());
    }

    #[test]
    fn 非管理员非负责人不能改负责人() {
        let (_d10, s) = store();
        setup_admin(&s);
        create_member(&s, "李四", ROLE_MEMBER, "张三").unwrap();
        create_member(&s, "王五", ROLE_MEMBER, "张三").unwrap();
        seed_item(&s, "i1", Some("李四"));
        assert!(set_item_owner(&s, "i1", Some("王五"), "王五").is_err(), "普通成员无权改他人项");
        // 负责人自己可以把自己的活转出去
        assert!(set_item_owner(&s, "i1", Some("王五"), "李四").unwrap().is_some());
    }

    #[test]
    fn 不能指派给未登记或已停用的人() {
        let (_d11, s) = store();
        setup_admin(&s);
        seed_item(&s, "i1", None);
        assert!(set_item_owner(&s, "i1", Some("路人"), "张三").is_err(), "未登记不可指派");
        let m = create_member(&s, "李四", ROLE_MEMBER, "张三").unwrap();
        set_member_status(&s, &m.member_id, "disabled", "张三").unwrap();
        assert!(set_item_owner(&s, "i1", Some("李四"), "张三").is_err(), "停用不可指派");
    }

    #[test]
    fn 新建工作项默认负责人为创建人() {
        let (_d12, s) = store();
        setup_admin(&s);
        seed_item(&s, "i1", None);
        init_owner_on_create(s.conn(), "i1", "张三").unwrap();
        let owner: Option<String> = s
            .conn()
            .query_row("SELECT owner FROM work_item WHERE id='i1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(owner.as_deref(), Some("张三"));
        assert!(pending_owner_changes(s.conn()).unwrap().is_empty(), "初次指定不算变更");
    }

    // ---- 引擎层（第二层闸）：不经过命令层直接调 pct::update_item，也必须被同一判据拦住 ----

    fn pct_of(s: &Store, id: &str) -> i64 {
        s.conn()
            .query_row("SELECT progress_pct FROM work_item WHERE id=?1", params![id], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn 引擎层拦截非负责人改进度且库值不变() {
        let (_d13, s) = store();
        setup_admin(&s);
        create_member(&s, "李四", ROLE_MEMBER, "张三").unwrap();
        create_member(&s, "王五", ROLE_MEMBER, "张三").unwrap();
        seed_item(&s, "i1", Some("李四"));
        let out = crate::pct::update_item(&s, &crate::pct::ItemPatch {
            item_id: "i1".into(),
            progress_pct: Some(60),
            status: None,
            note: None,
            by: Some("王五".into()),
        })
        .unwrap();
        let reason = out.rejected_reason.expect("引擎层应拒绝");
        assert!(reason.contains("李四"), "原因需点名负责人：{reason}");
        assert_eq!(pct_of(&s, "i1"), 0, "被拒后不得写入");
        assert!(!out.action_logged, "被拒不得留动作记录");
    }

    #[test]
    fn 引擎层对负责人与管理员放行() {
        let (_d14, s) = store();
        setup_admin(&s);
        create_member(&s, "李四", ROLE_MEMBER, "张三").unwrap();
        seed_item(&s, "i1", Some("李四"));
        // 负责人本人
        let a = crate::pct::update_item(&s, &crate::pct::ItemPatch {
            item_id: "i1".into(), progress_pct: Some(30), status: None, note: None,
            by: Some("李四".into()),
        })
        .unwrap();
        assert!(a.rejected_reason.is_none(), "{:?}", a.rejected_reason);
        assert_eq!(pct_of(&s, "i1"), 30);
        // 管理员（张三）
        let b = crate::pct::update_item(&s, &crate::pct::ItemPatch {
            item_id: "i1".into(), progress_pct: Some(55), status: None, note: None,
            by: Some("张三".into()),
        })
        .unwrap();
        assert!(b.rejected_reason.is_none(), "{:?}", b.rejected_reason);
        assert_eq!(pct_of(&s, "i1"), 55);
    }

    #[test]
    fn 操作人传成员id时按显示名判定() {
        // 命令层传进来的是 member_id（登录写入的那个），负责人字段存的是显示名。
        // 不归一的话负责人本人会被自己的活挡在门外。
        let (_d16, s) = store();
        setup_admin(&s);
        let li = create_member(&s, "李四", ROLE_MEMBER, "张三").unwrap();
        let wang = create_member(&s, "王五", ROLE_MEMBER, "张三").unwrap();
        seed_item(&s, "i1", Some("李四"));
        assert_eq!(deny_reason(s.conn(), "i1", &li.member_id), None, "负责人按 id 传也应放行");
        let r = deny_reason(s.conn(), "i1", &wang.member_id).expect("非负责人应被拒");
        assert!(r.contains("李四"), "{r}");
        // 负责人变更同理：按 id 传的负责人本人可以把自己的活转出去
        assert!(set_item_owner(&s, "i1", Some("王五"), &li.member_id).unwrap().is_some());
    }

    #[test]
    fn 引擎层未传操作人时放行以保住单机老数据() {
        let (_d15, s) = store();
        setup_admin(&s);
        seed_item(&s, "i1", Some("李四"));
        let out = crate::pct::update_item(&s, &crate::pct::ItemPatch {
            item_id: "i1".into(), progress_pct: Some(40), status: None, note: None, by: None,
        })
        .unwrap();
        assert!(out.rejected_reason.is_none());
        assert_eq!(pct_of(&s, "i1"), 40);
    }
}
