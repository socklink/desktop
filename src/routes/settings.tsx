import { createFileRoute, useNavigate } from '@tanstack/react-router'
import { useState, useEffect } from 'react'
import { ChevronLeft, Monitor, Wifi, Power, Info } from 'lucide-react'
import { invoke } from '@tauri-apps/api/core'
import { getVersion } from '@tauri-apps/api/app'
import { isEnabled, enable, disable } from '@tauri-apps/plugin-autostart'

export const Route = createFileRoute('/settings')({
  component: SettingsPage,
})

function SettingsPage() {
  const navigate = useNavigate({ from: Route.fullPath })
  const [ip, setIp] = useState<string | null>(null)
  const [name, setName] = useState<string | null>(null)
  const [version, setVersion] = useState<string>('…')
  const [autostart, setAutostart] = useState<boolean | null>(null)

  useEffect(() => {
    invoke<string>('get_device_ip').then(setIp).catch(() => {})
    invoke<string>('get_device_name').then(setName).catch(() => {})
    getVersion().then(setVersion).catch(() => {})
    isEnabled().then(setAutostart).catch(() => {})
  }, [])

  const toggleAutostart = async () => {
    if (autostart === null) return
    try {
      if (autostart) {
        await disable()
        setAutostart(false)
      } else {
        await enable()
        setAutostart(true)
      }
    } catch (err) {
      console.error('Failed to toggle autostart:', err)
    }
  }

  return (
    <div className="app-shell">
      <header className="top-bar">
        <div className="top-bar-left">
          <button
            className="top-bar-action-btn"
            onClick={() => navigate({ to: '/' })}
            title="Back to dashboard"
          >
            <ChevronLeft size={18} strokeWidth={2.5} />
          </button>
          <div>
            <div className="page-title" style={{ fontSize: 17, fontWeight: 700, lineHeight: 1.2 }}>Settings</div>
            <div style={{ fontSize: 12, color: 'var(--text-tertiary)' }}>Desktop application preferences and network details</div>
          </div>
        </div>
      </header>

      <main className="main-content">
        <div className="dashboard-wrap" style={{ maxWidth: 720, padding: '32px 24px 48px 24px', gap: 36 }}>

          <section>
            <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 14 }}>
              <Power size={18} style={{ color: 'var(--text-secondary)' }} />
              <div style={{ fontSize: 15, fontWeight: 600, color: 'var(--text-primary)' }}>Startup & System</div>
            </div>
            <div style={{ display: 'flex', flexDirection: 'column' }}>
              <div style={{
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'space-between',
                padding: '16px 0',
                borderBottom: '1px solid var(--border)',
              }}>
                <div>
                  <div style={{ fontSize: 14, fontWeight: 500, color: 'var(--text-primary)' }}>Run at Startup</div>
                  <div style={{ fontSize: 12.5, color: 'var(--text-tertiary)', marginTop: 2 }}>
                    Automatically launch SockLink background service when your computer boots up
                  </div>
                </div>
                <button
                  onClick={toggleAutostart}
                  className="btn"
                  style={{
                    padding: '6px 14px',
                    fontSize: 12.5,
                    fontWeight: 600,
                    background: autostart ? 'var(--success-soft)' : 'var(--danger-soft)',
                    color: autostart ? 'var(--success)' : 'var(--danger)',
                    border: `1px solid ${autostart ? 'rgba(52,211,153,0.25)' : 'rgba(248,113,113,0.25)'}`,
                    borderRadius: 'var(--radius-md)',
                    cursor: 'pointer',
                    minWidth: 90,
                    textAlign: 'center'
                  }}
                >
                  {autostart === null ? '…' : autostart ? 'Enabled' : 'Disabled'}
                </button>
              </div>
            </div>
          </section>

          <section>
            <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 14 }}>
              <Monitor size={18} style={{ color: 'var(--text-secondary)' }} />
              <div style={{ fontSize: 15, fontWeight: 600, color: 'var(--text-primary)' }}>Device Information</div>
            </div>
            <div style={{ display: 'flex', flexDirection: 'column' }}>
              {[
                { label: 'Device Name', value: name ?? '…' },
                { label: 'Local IP Address', value: ip ?? '…' },
              ].map((row) => (
                <div key={row.label} style={{
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'space-between',
                  padding: '14px 0',
                  borderBottom: '1px solid var(--border)',
                }}>
                  <span style={{ fontSize: 14, color: 'var(--text-secondary)' }}>{row.label}</span>
                  <span style={{
                    fontSize: 13,
                    fontFamily: 'JetBrains Mono, monospace',
                    color: 'var(--text-primary)',
                    fontWeight: 500,
                  }}>{row.value}</span>
                </div>
              ))}
            </div>
          </section>

          <section>
            <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 14 }}>
              <Wifi size={18} style={{ color: 'var(--text-secondary)' }} />
              <div style={{ fontSize: 15, fontWeight: 600, color: 'var(--text-primary)' }}>Network Protocol</div>
            </div>
            <div style={{ display: 'flex', flexDirection: 'column' }}>
              {[
                { label: 'UDP Discovery Port', value: '7890' },
                { label: 'TCP Data Port', value: '7891' },
                { label: 'Transport Security', value: 'Encrypted Token Handshake' },
              ].map((row) => (
                <div key={row.label} style={{
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'space-between',
                  padding: '14px 0',
                  borderBottom: '1px solid var(--border)',
                }}>
                  <span style={{ fontSize: 14, color: 'var(--text-secondary)' }}>{row.label}</span>
                  <span style={{
                    fontSize: 13,
                    fontFamily: 'JetBrains Mono, monospace',
                    color: 'var(--text-primary)',
                    fontWeight: 500,
                  }}>{row.value}</span>
                </div>
              ))}
            </div>
          </section>

          <section>
            <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 14 }}>
              <Info size={18} style={{ color: 'var(--text-secondary)' }} />
              <div style={{ fontSize: 15, fontWeight: 600, color: 'var(--text-primary)' }}>About SockLink</div>
            </div>
            <div style={{ display: 'flex', flexDirection: 'column' }}>
              {[
                { label: 'Version', value: version },
                { label: 'Platform', value: 'Desktop (Tauri 2 + React)' },
              ].map((row) => (
                <div key={row.label} style={{
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'space-between',
                  padding: '14px 0',
                  borderBottom: '1px solid var(--border)',
                }}>
                  <span style={{ fontSize: 14, color: 'var(--text-secondary)' }}>{row.label}</span>
                  <span style={{
                    fontSize: 13,
                    fontFamily: 'JetBrains Mono, monospace',
                    color: 'var(--text-primary)',
                    fontWeight: 500,
                  }}>{row.value}</span>
                </div>
              ))}
            </div>
          </section>

        </div>
      </main>
    </div>
  )
}
