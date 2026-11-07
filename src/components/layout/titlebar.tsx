import { getCurrentWindow } from '@tauri-apps/api/window'
import logo from '../../assets/logo-full.png'

export function Titlebar() {
  const appWindow = getCurrentWindow()

  const handleMinimize = () => {
    appWindow.minimize().catch((err) => console.error('Minimize failed:', err))
  }

  const handleClose = () => {
    appWindow.close().catch((err) => console.error('Close failed:', err))
  }

  return (
    <div className="titlebar">
      <div className="titlebar-drag-region" data-tauri-drag-region>
        <div className="titlebar-logo" data-tauri-drag-region>
          <img
            src={logo}
            alt="SockLink Logo"
            className="titlebar-logo-img"
            data-tauri-drag-region
          />
          <span className="titlebar-logo-text" data-tauri-drag-region>
            SockLink
          </span>
        </div>
      </div>

      <div className="titlebar-controls">
        {/* Minimize Button */}
        <button
          onClick={handleMinimize}
          className="titlebar-btn"
          title="Minimize"
          aria-label="Minimize"
        >
          <svg width="10" height="1" viewBox="0 0 10 1" fill="none" stroke="currentColor">
            <line x1="0" y1="0.5" x2="10" y2="0.5" strokeWidth="1" />
          </svg>
        </button>

        {/* Close Button */}
        <button
          onClick={handleClose}
          className="titlebar-btn titlebar-btn-close"
          title="Close"
          aria-label="Close"
        >
          <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor">
            <path d="M1 1L9 9M9 1L1 9" strokeWidth="1" />
          </svg>
        </button>
      </div>
    </div>
  )
}
