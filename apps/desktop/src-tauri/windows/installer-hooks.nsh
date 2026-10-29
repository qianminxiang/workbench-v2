; ─────────────────────────────────────────────────────────────────────────────
; 综合工作台 · v2.2.1 存量迁移钩子（产品名「个人工作台」→「综合工作台」）
;
; 为什么必须有这个文件：
;   tauri NSIS 模板把产品名在编译期焊进三条键路径（installer.nsi L58/L60/L78）：
;     UNINSTKEY       = Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}
;     MANUPRODUCTKEY  = Software\${MANUFACTURER}\${PRODUCTNAME}
;     PLACEHOLDER_INSTALL_DIR = placeholder\${PRODUCTNAME}
;   且自动更新走的 `/UPDATE` 模式**永不卸载旧版**（L295-296：$UpdateMode=1 跳过卸载），
;   升级模式**不创建快捷方式**（L916-918 / L945-946：$UpdateMode=1 提前 Return）。
;   产品名一改，老用户升级的默认结果就是：两套卸载项 + 两个安装目录 + 旧桌面图标残留指向旧目录。
;   旧目录里同样有 wb-desktop.exe 与 uninstall.exe，而 identifier（com.anhui.pr.workbench）
;   保持不变 → 两份安装共用同一把单实例锁，表现为「点图标没反应」的随机假故障。
;   本钩子即在安装前把旧注册项、旧目录、旧快捷方式清掉，把「两份并存」压回「一份」。
;
; 数据安全边界（先看清再改）：
;   用户数据目录由 tauri `app_data_dir()` 决定 = %APPDATA%\com.anhui.pr.workbench，
;   与安装目录（%LOCALAPPDATA%\个人工作台，只含二进制与 uninstall.exe）不是同一路径。
;   identifier 未变 → 数据目录不迁、不碰。本文件只删「旧安装目录」，绝不删 %APPDATA%。
;
; 旧发行者有两个（历史遗留，两个都要认）：
;   v2.1.0 bundle.publisher = 安徽大区小团队
;   v2.2.0 bundle.publisher = 安徽省小团队
;   → MANUPRODUCTKEY 随之各异。旧产品名只能硬编码在这里：新版编译期的
;     ${PRODUCTNAME} 已经是「综合工作台」，无法再用来指代旧名。
; ─────────────────────────────────────────────────────────────────────────────

!define WB_OLD_PRODUCTNAME "个人工作台"
!define WB_OLD_MANU_V210 "安徽大区小团队"
!define WB_OLD_MANU_V220 "安徽省小团队"

; ── 旧 MSI：枚举 Uninstall 子键，按下标名（即 ProductCode GUID）调 msiexec /x ──
; 为什么要单独处理：模板 PageReinstall 的 FindWixInstall（L170-186）靠
; DisplayName==${PRODUCTNAME} 且 Publisher==${MANUFACTURER} 匹配旧 MSI；
; 改名 + 改发行者后这两个条件对旧 MSI 全部不成立，那条路径已结构性断裂。
; 不在这里清掉，MSI 老用户升级后就是 Program Files 与 LocalAppData 两份并存。
; 参数 $R8 = 待枚举的 Uninstall 根路径。
Function WB_PurgeOldMsiUnder
  StrCpy $2 0
  wb_msi_loop:
    EnumRegKey $1 "$R8" $2
    StrCmp $1 "" wb_msi_done
    IntOp $2 $2 + 1
    ReadRegStr $R0 "$R8\$1" "DisplayName"
    StrCmp $R0 "${WB_OLD_PRODUCTNAME}" 0 wb_msi_loop
    ReadRegStr $R3 "$R8\$1" "Publisher"
    StrCmp $R3 "${WB_OLD_MANU_V220}" wb_msi_hit
    StrCmp $R3 "${WB_OLD_MANU_V210}" wb_msi_hit
    Goto wb_msi_loop
  wb_msi_hit:
    ReadRegStr $R4 "$R8\$1" "UninstallString"
    ${StrLoc} $R5 $R4 "msiexec" ">"
    StrCmp $R5 "" wb_msi_loop
    DetailPrint "清理旧 MSI 安装：键=$1"
    ExecWait '"$SYSDIR\msiexec.exe" /x $1 /qn /norestart' $R6
    ; 1603/1925 之类多因权限不足：不中断安装，但要留痕，不假装清干净了
    DetailPrint "msiexec /x 退出码=$R6（0=成功，3010=需重启，1603/1925=权限不足）"
    Goto wb_msi_loop
  wb_msi_done:
FunctionEnd

!macro WB_PurgeOldMsi
  StrCpy $R8 "HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall"
  Call WB_PurgeOldMsiUnder
  StrCpy $R8 "HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"
  Call WB_PurgeOldMsiUnder
  StrCpy $R8 "HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall"
  Call WB_PurgeOldMsiUnder
!macroend

; ── 旧 NSIS：卸载项（HKCU/HKLM）+ 制造者键（两个发行者）+ 自启动值 ──
!macro WB_PurgeOldRegistration
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${WB_OLD_PRODUCTNAME}"
  ClearErrors
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${WB_OLD_PRODUCTNAME}"
  ClearErrors
  DeleteRegKey HKCU "Software\${WB_OLD_MANU_V220}\${WB_OLD_PRODUCTNAME}"
  ClearErrors
  DeleteRegKey HKLM "Software\${WB_OLD_MANU_V220}\${WB_OLD_PRODUCTNAME}"
  ClearErrors
  DeleteRegKey HKCU "Software\${WB_OLD_MANU_V210}\${WB_OLD_PRODUCTNAME}"
  ClearErrors
  DeleteRegKey HKLM "Software\${WB_OLD_MANU_V210}\${WB_OLD_PRODUCTNAME}"
  ClearErrors
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${WB_OLD_PRODUCTNAME}"
  ClearErrors
!macroend

; ── 旧 NSIS：快捷方式。旧版 STARTMENUFOLDER 为空，图标平铺在 $SMPROGRAMS 根下，
;    这里同时兜一层子文件夹形态（历史手工安装可能建过）──
!macro WB_PurgeOldShortcuts
  Delete "$DESKTOP\${WB_OLD_PRODUCTNAME}.lnk"
  Delete "$SMPROGRAMS\${WB_OLD_PRODUCTNAME}.lnk"
  RMDir /r "$SMPROGRAMS\${WB_OLD_PRODUCTNAME}"
!macroend

; ── 旧 NSIS：安装目录。先确认旧主程序已退出，否则文件占用会让删除半途失败。
;    用模板同款 CheckIfAppIsRunning（按 exe 名 wb-desktop.exe 查杀：静默/被动自动杀、
;    交互模式给用户确认），与模板 L621 同一手段 —— 旧名与新名的 MAINBINARYNAME 相同，
;    故单实例锁也一并解除。
!macro WB_PurgeOldInstallDir
  !insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}"
  ${If} ${FileExists} "$LOCALAPPDATA\${WB_OLD_PRODUCTNAME}\${MAINBINARYNAME}.exe"
    DetailPrint "清理旧安装目录：$LOCALAPPDATA\${WB_OLD_PRODUCTNAME}"
    RMDir /r "$LOCALAPPDATA\${WB_OLD_PRODUCTNAME}"
  ${EndIf}
  ${If} ${FileExists} "$LOCALAPPDATA\${WB_OLD_PRODUCTNAME}\${MAINBINARYNAME}.exe"
    ; 删不掉必须写进安装日志，不静默放过
    DetailPrint "警告：旧安装目录仍有残留（进程占用）：$LOCALAPPDATA\${WB_OLD_PRODUCTNAME}"
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  ; 旧 NSIS 安装判定：注册项还在、或旧目录还在（用户可能手工删过一侧）
  ReadRegStr $R9 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${WB_OLD_PRODUCTNAME}" "UninstallString"
  ${If} $R9 != ""
  ${OrIf} ${FileExists} "$LOCALAPPDATA\${WB_OLD_PRODUCTNAME}\${MAINBINARYNAME}.exe"
    DetailPrint "检测到旧产品名「${WB_OLD_PRODUCTNAME}」安装，执行存量迁移"
    !insertmacro WB_PurgeOldRegistration
    !insertmacro WB_PurgeOldShortcuts
    !insertmacro WB_PurgeOldInstallDir
  ${Else}
    DetailPrint "未检测到旧 NSIS 安装，跳过"
  ${EndIf}
  ; 旧 MSI 独立判定：可能只装了 MSI（旧 MSI 的 DisplayName 同样是旧产品名）
  !insertmacro WB_PurgeOldMsi
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; /UPDATE 分支下模板不创建快捷方式（L916-918 / L945-946）。
  ; 旧名图标已在 PREINSTALL 删除，这里不补建，自动更新用户升级后就没有桌面图标 ——
  ; 这是改名最容易漏掉的一处连带损失。按当前产品名重建，指向新安装目录。
  ${If} $UpdateMode = 1
    DetailPrint "更新模式：补建「${PRODUCTNAME}」快捷方式"
    Delete "$DESKTOP\${PRODUCTNAME}.lnk"
    CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
    Delete "$SMPROGRAMS\${PRODUCTNAME}.lnk"
    CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\${PRODUCTNAME}.lnk"
  ${EndIf}
!macroend
