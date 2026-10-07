import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { ArrowRight, Check, Download, Languages, LayoutDashboard, Minus, Play, RefreshCw, Settings2, X } from 'lucide-react'
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { RefObject } from 'react'
import './App.css'

type Page = 'startup' | 'setup' | 'overview' | 'localize' | 'operation'
type InstallMode = 'portable' | 'user' | 'system'
type PatchMode = 'safe' | 'full'
type Platform = 'windows' | 'macos'
type OperationKind =
  | 'install_assistant' | 'install_claude' | 'check_claude_update' | 'update_claude'
  | 'apply_patch' | 'restore_patch' | 'create_claude_shortcut'
  | 'check_assistant_update' | 'install_assistant_update' | 'uninstall_assistant'

type ClaudeStatus = {
  installed: boolean
  version: string
  packageFullName: string
  installPath: string
  appliedMode: PatchMode | null
  backupReady: boolean
  externalLocalization: boolean
  patchRecoveryRequired: boolean
  message: string
  updateAvailable?: boolean | null
  latestVersion?: string | null
  updateCheckError?: string | null
  lastUpdateCheck?: string | null
}
type AssistantStatus = {
  installed: boolean
  mode: InstallMode | null
  installPath: string
  version: string
  shortcutReady: boolean
  defaultPaths: Record<InstallMode, string>
  updateAvailable?: boolean | null
  latestVersion?: string | null
  updateCheckError?: string | null
  lastUpdateCheck?: string | null
  updateResult?: string | null
  updateResultOk?: boolean | null
  uninstallResult?: string | null
  uninstallResultOk?: boolean | null
}
type AssistantConfig = {
  assistantInstallMode: InstallMode
  assistantPath: string
  createAssistantShortcut: boolean
  dailyUpdateCheck: boolean
}
type OperationSnapshot = {
  id: number
  kind: OperationKind
  step: string
  progress: number | null
  logs: string[]
  state: 'idle' | 'running' | 'success' | 'error'
  message: string
  updateAvailable: boolean | null
  latestVersion: string | null
}
type ConfirmState = {
  title: string
  text: string
  accept: string
  action: () => void
}

const runningInTauri = '__TAURI_INTERNALS__' in window
const operationTitles: Record<OperationKind, string> = {
  install_assistant: '正在安装助手',
  install_claude: '正在安装 Claude',
  check_claude_update: '正在检查 Claude 更新',
  update_claude: '正在更新 Claude',
  apply_patch: '正在汉化 Claude',
  restore_patch: '正在恢复原样',
  create_claude_shortcut: '正在创建 Claude 快捷方式',
  check_assistant_update: '正在检查助手更新',
  install_assistant_update: '正在更新助手',
  uninstall_assistant: '正在卸载助手',
}
const confirmationLabels: Partial<Record<OperationKind, string>> = {
  apply_patch: '关闭 Claude 并汉化',
  restore_patch: '恢复原样',
  install_claude: '安装 Claude',
  update_claude: '安装更新',
  install_assistant_update: '安装更新',
  uninstall_assistant: '卸载助手',
}

const defaultConfig: AssistantConfig = {
  assistantInstallMode: 'user',
  assistantPath: '',
  createAssistantShortcut: true,
  dailyUpdateCheck: true,
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

function errorSummary(message: string): string {
  const firstLine = message.split(/\r\n?|\n/).find((line) => line.trim())?.trim() || '操作失败'
  return firstLine.length > 100 ? `${firstLine.slice(0, 99)}…` : firstLine
}

function returnPage(kind: OperationKind): 'overview' | 'localize' | 'settings' {
  if (kind === 'apply_patch' || kind === 'restore_patch') return 'localize'
  if (kind === 'check_assistant_update' || kind === 'install_assistant_update' || kind === 'create_claude_shortcut' || kind === 'uninstall_assistant') return 'settings'
  return 'overview'
}

function useDialogFocus(open: boolean, ref: RefObject<HTMLElement | null>, close: () => void) {
  useLayoutEffect(() => {
    const dialog = ref.current
    if (!open || !dialog) return
    const trigger = document.activeElement instanceof HTMLElement ? document.activeElement : null
    const controls = () => [...dialog.querySelectorAll<HTMLElement>('button, input, select, textarea, a[href], [tabindex]')]
      .filter((element) => element.tabIndex >= 0 && !element.matches(':disabled') && element.getClientRects().length > 0)
    const focusFirst = () => (controls()[0] ?? dialog).focus()
    focusFirst()
    const onKeyDown = (event: KeyboardEvent) => {
      if (dialog.inert) return
      if (event.key === 'Escape') {
        event.preventDefault()
        event.stopImmediatePropagation()
        close()
      } else if (event.key === 'Tab') {
        const items = controls()
        const index = items.indexOf(document.activeElement as HTMLElement)
        if (index < 0 || (event.shiftKey ? index === 0 : index === items.length - 1)) {
          event.preventDefault()
          const next = (event.shiftKey ? items.at(-1) : items[0]) ?? dialog
          next.focus()
        }
      }
    }
    const onFocus = (event: FocusEvent) => {
      // Native window controls remain usable even while a content dialog is open.
      if (!dialog.inert && event.target instanceof HTMLElement && !dialog.contains(event.target) && !event.target.closest('.ca-titlebar')) focusFirst()
    }
    document.addEventListener('keydown', onKeyDown)
    document.addEventListener('focusin', onFocus)
    return () => {
      document.removeEventListener('keydown', onKeyDown)
      document.removeEventListener('focusin', onFocus)
      if (trigger?.isConnected && !trigger.closest('[inert]')) trigger.focus()
    }
  }, [open, ref, close])
}

function App() {
  const [platform, setPlatform] = useState<Platform>(navigator.platform.startsWith('Mac') ? 'macos' : 'windows')
  const [assistant, setAssistant] = useState<AssistantStatus | null>(null)
  const [claude, setClaude] = useState<ClaudeStatus | null>(null)
  const [config, setConfig] = useState(defaultConfig)
  const [page, setPage] = useState<Page>('startup')
  const [selectedMode, setSelectedMode] = useState<PatchMode>('safe')
  const [operation, setOperation] = useState<OperationSnapshot | null>(null)
  const [retry, setRetry] = useState<{ kind: OperationKind; mode?: PatchMode } | null>(null)
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [logsExpanded, setLogsExpanded] = useState(false)
  const [confirm, setConfirm] = useState<ConfirmState | null>(null)
  const [actionError, setActionError] = useState('')
  const [settingsNotice, setSettingsNotice] = useState('')
  const [loadError, setLoadError] = useState('')
  const handledOperation = useRef<number | null>(null)
  const logRef = useRef<HTMLDivElement>(null)
  const followLogs = useRef(true)
  const logOperationId = useRef<number | undefined>(undefined)
  const startupAttempt = useRef(0)
  const startupTimer = useRef<number | null>(null)
  const settingsRef = useRef<HTMLElement>(null)
  const confirmRef = useRef<HTMLElement>(null)
  const errorRef = useRef<HTMLElement>(null)
  const statusErrorOpen = Boolean(loadError) && page !== 'startup'
  const errorOpen = Boolean(actionError) || statusErrorOpen
  const closeError = useCallback(() => setActionError(''), [])
  const closeSettings = useCallback(() => setSettingsOpen(false), [])
  const closeConfirm = useCallback(() => setConfirm(null), [])
  useDialogFocus(settingsOpen, settingsRef, closeSettings)
  useDialogFocus(confirm !== null, confirmRef, closeConfirm)
  useDialogFocus(errorOpen, errorRef, closeError)
  const busy = operation?.state === 'running'
  const operationId = operation?.id
  const activePage = page === 'operation' ? returnPage(operation?.kind ?? 'check_claude_update') : page
  const logsOpen = logsExpanded || operation?.state === 'error'
  const assistantUpdate = assistant?.updateAvailable == null ? null : { available: assistant.updateAvailable, version: assistant.latestVersion ?? '' }
  const title = page === 'startup' ? 'Claude 中文助手' : page === 'setup' ? '安装 Claude 中文助手' : page === 'overview' ? '概览' : page === 'localize' ? '汉化' : operation?.state === 'error' ? '操作失败' : operation ? operationTitles[operation.kind] : '执行'

  const refreshStatus = useCallback(async () => {
    const [assistantResult, claudeResult] = await Promise.allSettled([
      invoke<AssistantStatus>('get_assistant_status'),
      invoke<ClaudeStatus>('get_status'),
    ])
    const errors: string[] = []
    if (assistantResult.status === 'fulfilled') {
      setAssistant(assistantResult.value)
    }
    else errors.push(`助手状态：${errorText(assistantResult.reason)}`)
    if (claudeResult.status === 'fulfilled') {
      setClaude(claudeResult.value)
    }
    else errors.push(`Claude 状态：${errorText(claudeResult.reason)}`)
    setLoadError(errors.join('；'))
    if (errors.length) throw new Error(errors.join('；'))
    return assistantResult.status === 'fulfilled' ? assistantResult.value : null
  }, [])

  const loadStartup = useCallback(() => {
    setPage('startup')
    setActionError('')
    setSettingsOpen(false)
    setConfirm(null)
    if (!runningInTauri) {
      setLoadError('请运行 Claude 中文助手桌面程序。')
      return
    }
    const attempt = ++startupAttempt.current
    if (startupTimer.current !== null) window.clearTimeout(startupTimer.current)
    setLoadError('')
    startupTimer.current = window.setTimeout(() => {
      if (attempt === startupAttempt.current) {
        startupAttempt.current++
        startupTimer.current = null
        setLoadError('读取助手状态超时，请重新检测。')
      }
    }, 10_000)
    void Promise.allSettled([
      invoke<AssistantStatus>('get_assistant_status'),
      invoke<ClaudeStatus>('get_status'),
      invoke<AssistantConfig>('get_config'),
      invoke<OperationSnapshot>('get_operation_snapshot'),
      invoke<Platform>('get_platform'),
    ]).then(([assistantResult, claudeResult, configResult, operationResult, platformResult]) => {
      if (attempt !== startupAttempt.current) return
      const errors: string[] = []
      if (assistantResult.status === 'rejected') errors.push(`助手状态：${errorText(assistantResult.reason)}`)
      if (claudeResult.status === 'rejected') errors.push(`Claude 状态：${errorText(claudeResult.reason)}`)
      if (configResult.status === 'rejected') errors.push(`配置：${errorText(configResult.reason)}`)
      if (operationResult.status === 'rejected') errors.push(`执行状态：${errorText(operationResult.reason)}`)
      if (platformResult.status === 'rejected') errors.push(`系统平台：${errorText(platformResult.reason)}`)
      if (errors.length) {
        setLoadError(errors.join('；'))
        return
      }
      if (assistantResult.status === 'fulfilled' && claudeResult.status === 'fulfilled' && configResult.status === 'fulfilled' && operationResult.status === 'fulfilled') {
        if (platformResult.status === 'fulfilled') setPlatform(platformResult.value)
        setAssistant(assistantResult.value)
        setClaude(claudeResult.value)
        setSelectedMode(claudeResult.value.appliedMode ?? 'safe')
        const loaded = configResult.value
        setConfig({ ...loaded, assistantPath: loaded.assistantPath || assistantResult.value.defaultPaths[loaded.assistantInstallMode] })
        const snapshot = operationResult.value
        if (snapshot.state === 'running' || snapshot.state === 'error') {
          setOperation(snapshot)
          setRetry(snapshot.kind === 'apply_patch' ? null : { kind: snapshot.kind })
          setPage('operation')
        } else setPage(assistantResult.value.installed ? 'overview' : 'setup')
      }
    }).finally(() => {
      if (attempt === startupAttempt.current && startupTimer.current !== null) {
        window.clearTimeout(startupTimer.current)
        startupTimer.current = null
      }
    })
  }, [])

  const cancelStartup = useCallback(() => {
    startupAttempt.current++
    if (startupTimer.current !== null) window.clearTimeout(startupTimer.current)
    startupTimer.current = null
  }, [])

  useEffect(() => {
    let cancelled = false
    // Reveal the committed loading screen before starting native status queries.
    const start = async () => {
      if (runningInTauri) {
        try {
          await invoke('fit_initial_window', { webScale: window.devicePixelRatio })
          await getCurrentWindow().show()
        } catch (error) {
          console.error('无法初始化助手窗口', error)
          if (!cancelled) setLoadError(`无法初始化助手窗口：${errorText(error)}`)
          await getCurrentWindow().show().catch((showError) => console.error('无法显示助手窗口', showError))
          return
        }
      }
      if (!cancelled) loadStartup()
    }
    void start()
    return () => {
      cancelled = true
      cancelStartup()
    }
  }, [loadStartup, cancelStartup])

  useEffect(() => {
    if (!runningInTauri || !busy || !operationId || operationId <= 0) return
    let pending = false
    const poll = async () => {
      if (pending) return
      pending = true
      try {
        const snapshot = await invoke<OperationSnapshot>('get_operation_snapshot')
        setOperation((current) => current && current.id > snapshot.id ? current : snapshot)
      } catch (error) {
        setOperation((current) => current ? { ...current, state: 'error', step: '无法读取操作状态', message: errorText(error), logs: [...current.logs, errorText(error)] } : current)
      } finally {
        pending = false
      }
    }
    const timer = window.setInterval(() => { void poll() }, 500)
    return () => window.clearInterval(timer)
  }, [busy, operationId])

  useEffect(() => {
    if (!runningInTauri) return
    const timer = window.setInterval(() => {
      if (!busy && page !== 'startup') void refreshStatus().catch(() => {})
    }, 60_000)
    return () => window.clearInterval(timer)
  }, [busy, page, refreshStatus])

  useEffect(() => {
    if (!operation || operation.state !== 'success' || handledOperation.current === operation.id) return
    handledOperation.current = operation.id
    const finished = operation
    void refreshStatus().then((currentAssistant) => {
      if (finished.kind === 'uninstall_assistant') {
        setPage(currentAssistant?.installed ? 'overview' : 'setup')
      } else {
        setPage(returnPage(finished.kind) === 'localize' ? 'localize' : 'overview')
      }
      setSettingsOpen(returnPage(finished.kind) === 'settings')
      const deferred = (finished.kind === 'install_assistant_update' && finished.message.includes('替换进程已启动'))
        || (finished.kind === 'uninstall_assistant' && finished.message.includes('卸载脚本已启动'))
      const message = deferred ? `${finished.message}最终结果将在下次启动助手后显示。` : finished.message
      setSettingsNotice(deferred ? message : '')
      setOperation(null)
    }).catch((error) => {
      setLoadError(`操作结果已返回，但重新检测状态失败：${errorText(error)}`)
      setPage(assistant?.installed ? 'overview' : 'startup')
      setOperation(null)
    })
  }, [operation, refreshStatus, assistant?.installed])

  useLayoutEffect(() => {
    const element = logRef.current
    if (logOperationId.current !== operationId) {
      logOperationId.current = operationId
      followLogs.current = true
    }
    // Keep the user's follow choice from before this batch changed the scroll height.
    if (element && followLogs.current) element.scrollTop = element.scrollHeight
  }, [operationId, operation?.logs, logsOpen])

  const startOperation = useCallback(async (kind: OperationKind, mode?: PatchMode, before?: () => Promise<unknown>) => {
    setConfirm(null)
    setLogsExpanded(false)
    setSettingsOpen(false)
    setActionError('')
    setSettingsNotice('')
    setRetry({ kind, mode })
    handledOperation.current = null
    setPage('operation')
    setOperation({ id: -Date.now(), kind, step: '正在准备', progress: null, logs: [], state: 'running', message: '', updateAvailable: null, latestVersion: null })
    try {
      if (before) await before()
      const snapshot = await invoke<OperationSnapshot>('start_operation', { kind, mode: mode ?? null })
      setOperation(snapshot)
    } catch (error) {
      const message = errorText(error)
      setOperation({ id: -Date.now(), kind, step: message, progress: null, logs: [message], state: 'error', message, updateAvailable: null, latestVersion: null })
    }
  }, [])

  const installAssistant = () => {
    if (!config.assistantPath.trim()) return
    void startOperation('install_assistant', undefined, async () => {
      const saved = await invoke<AssistantConfig>('save_config', { config })
      setConfig(saved)
    })
  }

  const changeInstallMode = (mode: InstallMode) => {
    setConfig((current) => ({ ...current, assistantInstallMode: mode, assistantPath: assistant?.defaultPaths[mode] ?? '' }))
  }

  const chooseInstallPath = async () => {
    try {
      const path = await invoke<string | null>('choose_assistant_install_path')
      if (path) setConfig((current) => ({ ...current, assistantPath: path }))
    } catch (error) {
      setActionError(`无法选择安装位置：${errorText(error)}`)
    }
  }

  const changeDailyCheck = async (enabled: boolean) => {
    const next = { ...config, dailyUpdateCheck: enabled }
    try {
      const saved = await invoke<AssistantConfig>('save_config', { config: next })
      setConfig(saved)
      setSettingsNotice('')
    } catch (error) {
      setActionError(`保存设置失败：${errorText(error)}`)
    }
  }

  const openClaude = async () => {
    setActionError('')
    try {
      await invoke<void>('open_claude')
    } catch (error) {
      setActionError(`无法打开 Claude Desktop：${errorText(error)}`)
    }
  }

  const controlWindow = async (action: 'minimize' | 'close', label: string) => {
    try {
      await getCurrentWindow()[action]()
    } catch (error) {
      setActionError(`无法${label}助手窗口：${errorText(error)}`)
    }
  }

  const closeOperation = () => {
    const kind = operation?.kind ?? 'check_claude_update'
    setOperation(null)
    setPage(returnPage(kind) === 'localize' ? 'localize' : 'overview')
    setSettingsOpen(returnPage(kind) === 'settings')
    void refreshStatus().catch(() => {})
  }

  const copyLogs = async () => {
    try {
      await navigator.clipboard.writeText(operation?.logs.join('\n') ?? '')
    } catch (error) {
      setOperation((current) => current ? { ...current, logs: [...current.logs, `复制日志失败：${errorText(error)}`] } : current)
    }
  }

  const confirmAction = (title: string, text: string, kind: OperationKind, mode?: PatchMode) => {
    setConfirm({ title, text, accept: confirmationLabels[kind] ?? title, action: () => { void startOperation(kind, mode) } })
  }

  const localizeCopy = !claude ? '正在读取 Claude 状态。' : !claude.installed ? '先安装 Claude，再选择是否汉化。' : claude.patchRecoveryRequired ? "上次汉化未完成，请前往汉化页查看核查位置。" : claude.externalLocalization ? '检测到其他来源的中文资源或备份，助手不会接管。' : claude.appliedMode ? `已应用汉化 · ${claude.appliedMode === 'full' ? '完整汉化' : 'Cowork 兼容'}` : '未应用汉化，按需开启。'
  const patchDisabled = busy || !claude?.installed || claude.externalLocalization || claude.patchRecoveryRequired
  const selectedApplied = claude?.appliedMode === selectedMode
  const applySelectedMode = () => confirmAction(`应用${selectedMode === 'safe' ? ' Cowork 兼容汉化' : '完整汉化'}？`, `将关闭 Claude，备份文件后应用汉化。${selectedMode === 'full' ? '完整汉化可能影响 Cowork。' : ''}`, 'apply_patch', selectedMode)
  const operationFailed = operation?.state === 'error'
  const operationProgress = operation?.progress
  const operationStep = operationFailed && operation
    ? errorSummary(operation.message || operation.step)
    : operation?.step.trim() === title ? '' : operation?.step.trim() ?? ''

  return (
    <div id="claude-assistant-sketch" data-platform={platform} aria-label="Claude 中文助手">
      <div className="ca-window">
        <div className="ca-titlebar" data-tauri-drag-region>
          <img src="/claude-icon.svg" alt="" data-tauri-drag-region />
          <span data-tauri-drag-region>Claude 中文助手</span>
          <span className="ca-window-controls">
            <button type="button" onClick={() => void controlWindow('minimize', '最小化')} aria-label="最小化"><Minus aria-hidden="true" /></button>
            <button type="button" onClick={() => void controlWindow('close', '关闭')} aria-label="关闭"><X aria-hidden="true" /></button>
          </span>
        </div>
        <div className="ca-layout" inert={settingsOpen || confirm !== null || errorOpen}>
          <aside className="ca-sidebar">
            <div className="ca-brand"><img src="/claude-icon.svg" alt="" /><span>Claude 中文助手</span></div>
            <nav className="ca-nav" aria-label="主导航">
              <button type="button" onClick={() => setPage('overview')} aria-current={activePage === 'overview' ? 'page' : undefined} disabled={busy || !assistant?.installed || page === 'startup' || Boolean(loadError)}><LayoutDashboard aria-hidden="true" /><span>概览</span></button>
              <button type="button" onClick={() => setPage('localize')} aria-current={activePage === 'localize' ? 'page' : undefined} disabled={busy || !assistant?.installed || page === 'startup' || Boolean(loadError)}><Languages aria-hidden="true" /><span>汉化</span></button>
            </nav>
          </aside>
          <main className="ca-main" data-page={page}>
            <header className="ca-heading">
              <div>
                <h1>{title}</h1>
                {page === 'overview' || page === 'localize' ? <p className="ca-heading-subtitle">{page === 'overview' ? '管理你的 Claude Desktop' : '选择适合你的中文体验'}</p> : null}
              </div>
              {assistant?.installed ? <button type="button" className="ca-icon-button" aria-label="设置" onClick={() => setSettingsOpen(true)} disabled={busy || page === 'operation' || page === 'startup'}><Settings2 aria-hidden="true" /></button> : null}
            </header>
            {page === 'startup' && loadError ? <div className="ca-startup"><p role="alert">{loadError}</p><button type="button" className="ca-button" onClick={loadStartup}>重新检测</button></div> : null}

            {page === 'startup' && !loadError ? <div className="ca-startup">
              <p role="status">正在读取助手状态…</p>
              <div className="ca-track" role="progressbar" aria-label="正在读取助手状态" data-indeterminate="true"><span /></div>
            </div> : null}

            {page === 'setup' ? <section className="ca-setup" aria-label="安装助手">
              <div className="ca-install-label"><label htmlFor="ca-install-mode">安装方式</label></div>
              <select id="ca-install-mode" className="ca-install-select" value={config.assistantInstallMode} onChange={(event) => changeInstallMode(event.target.value as InstallMode)} disabled={!assistant || busy}>
                <option value="portable">便携方式</option><option value="user">用户安装（推荐）</option><option value="system">系统安装</option>
              </select>
              <p className="ca-install-hint">{config.assistantInstallMode === 'portable' ? '默认在当前目录使用，不复制程序；选择其他目录时会保留原始文件。' : platform === 'macos' ? `将 Claude 中文助手.app 复制到所选目录${config.assistantInstallMode === 'system' ? '，系统安装可能需要管理员授权' : ''}；原始下载文件会保留。` : '程序将复制到安装位置；安装完成并退出后，可删除原始下载文件。'}</p>
              <div className="ca-install-label ca-install-path-label"><label htmlFor="ca-assistant-path">安装位置</label></div>
              <div className="ca-path-row">
                <input id="ca-assistant-path" value={config.assistantPath} aria-label="助手安装位置" onChange={(event) => setConfig((current) => ({ ...current, assistantPath: event.target.value }))} disabled={!assistant || busy} />
                <button type="button" className="ca-button" onClick={() => void chooseInstallPath()} disabled={!assistant || busy}>浏览…</button>
              </div>
              <label className="ca-install-check"><input type="checkbox" checked={config.createAssistantShortcut} onChange={(event) => setConfig((current) => ({ ...current, createAssistantShortcut: event.target.checked }))} disabled={!assistant || busy} />创建助手桌面快捷方式</label>
              <div className="ca-install-actions"><button type="button" className="ca-button ca-primary" onClick={installAssistant} disabled={!assistant || busy || !config.assistantPath.trim()}>安装</button></div>
            </section> : null}

            {page === 'overview' ? <section className="ca-pane ca-overview" aria-label="概览">
              <div className="ca-hero"><img src="/claude-icon.svg" className="ca-app-icon" alt="" /><div className="ca-hero-copy"><h2>Claude Desktop</h2><div className="ca-version">{claude ? claude.installed ? `${claude.version || '未知版本'} · 已安装` : '尚未安装' : '正在检测'}</div></div>
                <button type="button" className="ca-button ca-primary ca-launch" onClick={() => claude?.installed ? void openClaude() : confirmAction('安装 Claude', `将下载并安装官方 ${platform === 'macos' ? 'macOS' : 'Windows'} 应用。`, 'install_claude')} disabled={busy || !claude}>{claude?.installed ? <Play aria-hidden="true" /> : <Download aria-hidden="true" />}<span>{claude?.installed ? '打开 Claude' : '安装 Claude'}</span></button>
              </div>
              {claude?.updateAvailable && claude.installed ? <div className="ca-update"><div><h3>Claude 有新版本</h3><p>{claude.version || '当前版本'} → {claude.latestVersion}</p></div><button type="button" className="ca-button ca-primary" onClick={() => confirmAction('更新 Claude？', '安装过程中会关闭 Claude，请先保存正在进行的工作。', 'update_claude')} disabled={busy}><Download aria-hidden="true" />立即更新</button></div> : null}
              <div className="ca-row"><div><h3>应用更新</h3><p>{claude?.updateCheckError || (claude?.updateAvailable != null ? claude.updateAvailable ? '已发现新版本，可随时更新。' : '已是最新版本。' : '尚未检查最新版本')}</p></div><button type="button" className="ca-text-button" onClick={() => void startOperation('check_claude_update')} disabled={busy || !claude?.installed}><RefreshCw aria-hidden="true" />检查更新</button></div>
              <div className="ca-row"><div><h3>中文界面</h3><p>{localizeCopy}</p></div><button type="button" className="ca-text-button" onClick={() => setPage('localize')} disabled={busy || !claude?.installed}>前往汉化<ArrowRight aria-hidden="true" /></button></div>
            </section> : null}

            {page === 'localize' ? <section className="ca-pane ca-localize" aria-label="汉化">
              {claude?.patchRecoveryRequired || claude?.externalLocalization ? <div className="ca-label">{claude.patchRecoveryRequired ? claude.message : '检测到其他来源的中文资源或备份，助手不会接管。'}</div> : null}
              {(['safe', 'full'] as const).map((mode) => <label className="ca-mode" key={mode}><input type="radio" name="ca-patch-mode" checked={selectedMode === mode} onChange={() => setSelectedMode(mode)} disabled={busy} /><span><span className="ca-mode-title"><b>{mode === 'safe' ? 'Cowork 兼容' : '完整汉化'}</b>{mode === 'safe' ? <em>推荐</em> : null}{claude?.appliedMode === mode ? <span className="ca-applied" role="status"><Check aria-hidden="true" />已应用</span> : null}</span><p>{mode === 'safe' ? '汉化本地界面，优先保留 Cowork 兼容性。' : '同时汉化在线页面，可能影响 Cowork。'}</p></span></label>)}
              <div className="ca-actions">
                {selectedApplied ? <button type="button" className="ca-button ca-primary" onClick={() => void openClaude()} disabled={patchDisabled}><Play aria-hidden="true" />打开 Claude</button> : <button type="button" className="ca-button ca-primary" onClick={applySelectedMode} disabled={patchDisabled}><Languages aria-hidden="true" />应用此模式</button>}
                {selectedApplied ? <button type="button" className="ca-text-button" onClick={applySelectedMode} disabled={patchDisabled}>重新应用汉化</button> : null}
                <button type="button" className="ca-text-button" onClick={() => confirmAction('恢复原样？', '恢复当前 Claude 版本可验证的原始应用文件，保留登录和聊天数据。', 'restore_patch')} disabled={busy || !claude?.backupReady || claude.patchRecoveryRequired}>恢复原样</button>
              </div>
            </section> : null}

            {page === 'operation' && operation ? <section className="ca-operation" aria-label="执行进度" data-failed={operationFailed}>
              {operationStep || (operationProgress != null && !operationFailed) ? <div className="ca-operation-step"><span role="status">{operationStep}</span><span className="ca-operation-percent">{operationProgress != null && !operationFailed ? `${operationProgress}%` : ''}</span></div> : null}
              {!operationFailed ? <div className="ca-track" role="progressbar" aria-label="执行进度" aria-valuemin={0} aria-valuemax={100} aria-valuenow={operationProgress ?? undefined} data-indeterminate={operationProgress === null}><span style={operationProgress === null ? undefined : { width: `${operationProgress}%` }} /></div> : null}
              <div className="ca-log-tools"><button type="button" className="ca-text-button ca-log-toggle" aria-expanded={logsOpen} aria-controls="ca-operation-log" onClick={() => setLogsExpanded(!logsExpanded)} disabled={operationFailed}>{operationFailed ? '详细日志' : logsOpen ? '收起详细日志' : '展开详细日志'}</button><button type="button" className="ca-text-button ca-copy-log" onClick={() => void copyLogs()}>复制日志</button></div><div id="ca-operation-log" className="ca-log" hidden={!logsOpen} ref={logRef} onScroll={(event) => {
                const element = event.currentTarget
                followLogs.current = element.scrollHeight - element.scrollTop - element.clientHeight < 50
              }} role="log" aria-label="执行日志" aria-live="off" aria-busy={busy}>{operation.logs.map((line, index) => <div className="ca-log-line" data-error={operationFailed && index === operation.logs.length - 1} key={`${operation.id}-${index}`}><span>{line}</span></div>)}</div>
              {operationFailed ? <div className="ca-operation-actions"><button type="button" className="ca-button" onClick={closeOperation}>返回</button>{retry && !(operation.kind === 'apply_patch' && operation.message.includes('需人工核查')) ? <button type="button" className="ca-button ca-primary" onClick={() => void startOperation(retry.kind, retry.mode, retry.kind === 'install_assistant' ? () => invoke('save_config', { config }) : undefined)}>重试</button> : null}</div> : null}
            </section> : null}
          </main>
        </div>

        <div className="ca-overlay ca-settings-overlay" data-open={settingsOpen} aria-hidden={!settingsOpen}><section ref={settingsRef} tabIndex={-1} inert={!settingsOpen || confirm !== null || errorOpen} className="ca-settings" role="dialog" aria-modal="true" aria-labelledby="ca-settings-title">
          <div className="ca-settings-head"><h2 id="ca-settings-title">设置</h2><button type="button" className="ca-icon-button" aria-label="关闭设置" onClick={closeSettings}><X aria-hidden="true" /></button></div>
          <div className="ca-settings-body">
          <h3 className="ca-settings-group">常规</h3>
          <label className="ca-settings-row"><div><h3>自动检查更新</h3><p>助手运行时，每天检查 Claude 和助手更新。</p></div><input className="ca-switch" type="checkbox" role="switch" checked={config.dailyUpdateCheck} aria-label="每天自动检查更新" onChange={(event) => void changeDailyCheck(event.target.checked)} /></label>
          <div className="ca-settings-row"><div><h3>桌面快捷方式</h3><p>创建 Claude Desktop 桌面入口。</p></div><button type="button" className="ca-button" disabled={!claude?.installed} onClick={() => void startOperation('create_claude_shortcut')}>创建或修复</button></div>
          <section className="ca-settings-section" aria-labelledby="ca-about-title"><h3 id="ca-about-title" className="ca-settings-group">关于助手</h3>
            <div className="ca-settings-row"><div><h3>Claude 中文助手 <span className="ca-settings-version">{assistant?.version}</span></h3><p>{assistant?.updateCheckError || (assistantUpdate ? assistantUpdate.available ? `发现新版本 ${assistantUpdate.version}` : assistantUpdate.version ? '已是最新版本。' : '暂无可用的助手更新。' : assistant ? '检查助手的新版本。' : '正在读取版本')}</p></div><button type="button" className="ca-button" onClick={() => void startOperation('check_assistant_update')}>检查更新</button></div>
            {assistantUpdate?.available ? <div className="ca-settings-row"><div><h3>安装助手更新</h3><p>下载后验证摘要与程序自检。</p></div><button type="button" className="ca-button" onClick={() => confirmAction('更新助手？', '将校验下载文件并替换当前助手，失败时恢复旧文件。', 'install_assistant_update')}>安装更新</button></div> : null}
            {assistant?.updateResult ? <p className="ca-settings-message" data-error={assistant.updateResultOk === false} role={assistant.updateResultOk === false ? 'alert' : 'status'}>{assistant.updateResult}</p> : null}
            {settingsNotice && !settingsNotice.includes('卸载脚本已启动') ? <p className="ca-settings-message" role="status">{settingsNotice}</p> : null}
          </section>
          <div className="ca-settings-footer"><div className="ca-settings-footer-row"><p>卸载后保留 Claude 和个人数据</p><button type="button" className="ca-text-button ca-danger" onClick={() => confirmAction('卸载助手？', '仅移除助手及其创建的入口。Claude Desktop 与个人数据将保留。', 'uninstall_assistant')}>卸载助手</button></div>{assistant?.uninstallResult ? <p className="ca-settings-message" data-error={assistant.uninstallResultOk === false} role={assistant.uninstallResultOk === false ? 'alert' : 'status'}>{assistant.uninstallResult}</p> : null}{settingsNotice.includes('卸载脚本已启动') ? <p className="ca-settings-message" role="status">{settingsNotice}</p> : null}</div>
          </div>
        </section></div>

        {confirm ? <div className="ca-overlay"><section ref={confirmRef} tabIndex={-1} inert={errorOpen} className="ca-confirm" role="dialog" aria-modal="true" aria-labelledby="ca-confirm-title"><h2 id="ca-confirm-title">{confirm.title}</h2><p>{confirm.text}</p><div className="ca-actions"><button type="button" className="ca-button" onClick={closeConfirm}>取消</button><button type="button" className="ca-button ca-primary" onClick={confirm.action}>{confirm.accept}</button></div></section></div> : null}
        {errorOpen ? <div className="ca-overlay"><section ref={errorRef} tabIndex={-1} className="ca-confirm" role="alertdialog" aria-modal="true" aria-labelledby="ca-error-title" aria-describedby="ca-error-message"><h2 id="ca-error-title">{actionError ? '操作未完成' : '无法读取状态'}</h2><p id="ca-error-message">{actionError || loadError}</p><div className="ca-actions">{actionError ? <button type="button" className="ca-button ca-primary" onClick={closeError}>关闭</button> : <button type="button" className="ca-button ca-primary" onClick={loadStartup}>重新检测</button>}</div></section></div> : null}
      </div>
    </div>
  )
}

export default App
