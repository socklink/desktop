import { createFileRoute, useNavigate } from '@tanstack/react-router'
import { useState, useEffect } from 'react'
import { Wifi, Settings, Rocket, Smartphone, AlertTriangle } from 'lucide-react'
import { usePeers } from '../hooks/use-peers'
import { getBrand } from '../utils/device'
import { DeviceCard } from '../components/ui/device-card'
import { PairModal } from '../components/layout/pair-modal'
import { check } from '@tauri-apps/plugin-updater'
import { invoke } from '@tauri-apps/api/core'
import { isEnabled, enable } from '@tauri-apps/plugin-autostart'

export const Route = createFileRoute('/')({
  component: RouteComponent,
})

function RouteComponent() {
  const navigate = useNavigate({ from: Route.fullPath })
  const [updateAvailable, setUpdateAvailable] = useState<any>(null)
  const [updateStatus, setUpdateStatus] = useState<'idle' | 'checking' | 'updating' | 'installing' | 'error'>('idle')
  const [downloadProgress, setDownloadProgress] = useState(0)
  const [autostartEnabled, setAutostartEnabled] = useState<boolean | null>(null)

  useEffect(() => {
    const checkAutostart = async () => {
      try {
        const enabled = await isEnabled()
        setAutostartEnabled(enabled)
      } catch (err) {
        console.error('Failed to check autostart:', err)
      }
    }
    checkAutostart()
  }, [])

  const handleEnableAutostart = async () => {
    try {
      await enable()
      setAutostartEnabled(true)
    } catch (err) {
      console.error('Failed to enable autostart:', err)
    }
  }

  useEffect(() => {
    const checkUpdates = async () => {
      try {
        setUpdateStatus('checking')
        const update = await check()
        if (update && update.available) {
          setUpdateAvailable(update)
        }
        setUpdateStatus('idle')
      } catch (err) {
        console.error('Failed to check for updates:', err)
        setUpdateStatus('idle')
      }
    }
    checkUpdates()
  }, [])

  const handleUpdate = async () => {
    if (!updateAvailable) return
    try {
      setUpdateStatus('updating')
      setDownloadProgress(0)

      let total = 0
      let downloaded = 0

      await updateAvailable.downloadAndInstall((p: any) => {
        if (p.event === 'Started') {
          total = p.data.contentLength || 0
        } else if (p.event === 'Progress') {
          let length = p.data.chunkLength || 0
          downloaded += length
          if (total > 0) {
            let percentage = Math.round((downloaded / total) * 100)
            setDownloadProgress(percentage)
          }
        } else if (p.event === 'Finished') {
          setDownloadProgress(100)
          setUpdateStatus('installing')
        }
      })

      try {
        await invoke('relaunch_app')
      } catch (error: any) {
        console.error('Failed to relaunch:', error?.message || 'Unknown error')
        throw error
      }
    } catch (error: any) {
      console.error('Update failed:', error?.message || 'Unknown error')
      setUpdateStatus('error')
      alert(`Update failed: ${error?.message || 'Unknown error'}`)
    }
  }

  const {
    peers,
    pending,
    broadcasting,
    refreshPeers,
    resolvePairRequest,
    toggleBroadcasting,
  } = usePeers()

  const currentRequest = pending[0]

  return (
    <div className="app-shell">
      {/* ─── Top Bar ─────────────────────── */}
      <header className="top-bar">
        <div className="top-bar-left">
          <div className="top-bar-logo">
            <div className="top-bar-logo-badge">
              <svg
                xmlns="http://www.w3.org/2000/svg"
                viewBox="0 0 120 120"
                style={{ width: 18, height: 18, color: 'var(--text-primary)' }}
              >
                <path
                  d="M 7 86 A 53 53 0 0 1 113 86"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="10"
                  strokeLinecap="round"
                />
                <path
                  d="M 33 86 A 27 27 0 0 1 87 86"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="10"
                  strokeLinecap="round"
                />
                <circle cx="60" cy="86" r="6" fill="currentColor" />
              </svg>
            </div>
            <div style={{ display: 'flex', flexDirection: 'column', gap: 2 }}>
              <span className="top-bar-logo-text" style={{ fontSize: 15, fontWeight: 700, letterSpacing: '-0.3px', lineHeight: 1.1 }}>SockLink</span>
              <span style={{ fontSize: 9, fontWeight: 600, color: 'var(--text-tertiary)', letterSpacing: '0.6px', textTransform: 'uppercase', lineHeight: 1 }}>Desktop</span>
            </div>
          </div>
        </div>

        <div className="top-bar-right">
          {/* Custom styled pill broadcasting toggle */}
          <button
            onClick={toggleBroadcasting}
            className={`broadcasting-toggle ${broadcasting ? 'is-active' : ''}`}
            title={broadcasting ? 'Visible: Click to hide' : 'Hidden: Click to broadcast'}
          >
            <span className="status-dot-pulse" />
            <span>{broadcasting ? 'Visible' : 'Hidden'}</span>
          </button>

          <button className="top-bar-action-btn refresh-btn" onClick={refreshPeers} title="Refresh peers">
            <Wifi size={14} strokeWidth={2.5} />
          </button>

          <button className="top-bar-action-btn settings-btn" onClick={() => navigate({ to: '/settings' })} title="Settings">
            <Settings size={14} strokeWidth={2.5} />
          </button>
        </div>
      </header>

      {/* ─── Main Content ─────────────────── */}
      <main className="main-content">
        <div className="dashboard-wrap">
          {/* ─── Update Banner ─────────────────── */}
          {(updateAvailable || updateStatus === 'updating') && (
            <div style={{
              background: 'linear-gradient(135deg, rgba(6,182,212,0.08) 0%, rgba(180,142,247,0.04) 100%)',
              border: '1px solid rgba(6,182,212,0.2)',
              borderRadius: 'var(--radius-lg)',
              padding: '16px 20px',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'space-between',
              boxShadow: '0 8px 32px rgba(0,0,0,0.1)',
              position: 'relative',
              overflow: 'hidden'
            }}>
              <div style={{ position: 'absolute', top: -40, right: -40, width: 100, height: 100, background: 'var(--accent)', filter: 'blur(50px)', opacity: 0.15, pointerEvents: 'none' }} />

              <div>
                <div style={{ fontSize: 15, fontWeight: 600, color: '#fff', marginBottom: 4, display: 'flex', alignItems: 'center', gap: 8 }}>
                  <Rocket size={16} /> Update Available
                </div>
                <div style={{ fontSize: 13, color: 'var(--text-secondary)' }}>
                  Version {updateAvailable?.version || ''} is ready to install.
                </div>
              </div>

              {updateStatus === 'updating' || updateStatus === 'installing' ? (
                <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                  <div style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: 13, color: 'var(--accent)', background: 'rgba(6,182,212,0.1)', padding: '6px 14px', borderRadius: 'var(--radius-sm)', border: '1px solid rgba(6,182,212,0.2)' }}>
                    <span className="progress-spinner" style={{ width: 14, height: 14, borderWidth: 2 }} />
                    {updateStatus === 'updating' ? `Downloading... ${downloadProgress}%` : 'Installing...'}
                  </div>
                  {updateStatus === 'updating' && (
                    <div style={{ width: '100%', height: 4, background: 'rgba(255,255,255,0.1)', borderRadius: 2, overflow: 'hidden' }}>
                      <div style={{ height: '100%', width: `${downloadProgress}%`, background: 'var(--accent)', transition: 'width 0.2s' }} />
                    </div>
                  )}
                </div>
              ) : (
                <button
                  onClick={handleUpdate}
                  className="btn btn-primary"
                  style={{ padding: '8px 16px', background: 'var(--accent)', color: 'var(--bg-base)', border: 'none' }}
                >
                  Install Update
                </button>
              )}
            </div>
          )}

          {autostartEnabled === false && (
            <div style={{
              background: 'linear-gradient(135deg, rgba(234,179,8,0.1) 0%, rgba(245,158,11,0.05) 100%)',
              border: '1px solid rgba(245,158,11,0.3)',
              borderRadius: 'var(--radius-lg)',
              padding: '14px 18px',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'space-between',
              marginBottom: 16,
              boxShadow: '0 4px 20px rgba(0,0,0,0.15)',
            }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
                <div style={{
                  width: 34,
                  height: 34,
                  borderRadius: 'var(--radius-md)',
                  background: 'rgba(245,158,11,0.15)',
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'center',
                  color: 'var(--warning)',
                  flexShrink: 0
                }}>
                  <AlertTriangle size={18} />
                </div>
                <div>
                  <div style={{ fontSize: 14, fontWeight: 600, color: '#fff' }}>
                    Autostart Disabled
                  </div>
                  <div style={{ fontSize: 12.5, color: 'var(--text-secondary)' }}>
                    SockLink is not added to startup. Enable autostart so background sync connects automatically on boot.
                  </div>
                </div>
              </div>
              <button
                onClick={handleEnableAutostart}
                className="btn"
                style={{
                  padding: '6px 14px',
                  fontSize: 12.5,
                  fontWeight: 600,
                  background: 'var(--warning)',
                  color: '#000',
                  border: 'none',
                  borderRadius: 'var(--radius-md)',
                  cursor: 'pointer',
                  whiteSpace: 'nowrap'
                }}
              >
                Enable Autostart
              </button>
            </div>
          )}

          {/* Header */}
          <div className="page-header" style={{ marginBottom: 12 }}>
            <div>
              <div className="page-title">Paired Devices</div>
            </div>
          </div>

          {/* Device list */}
          <div className="section">
            {peers.length === 0 ? (
              <div className="empty-state">
                <div className="empty-state-icon"><Smartphone size={36} strokeWidth={1.5} /></div>
                <div className="empty-state-text">No paired devices yet</div>
                <div className="empty-state-sub">
                  Open the SockLink app on your Android and scan for this desktop
                </div>
              </div>
            ) : (
              <div className="device-list">
                {peers.map((peer) => (
                  <DeviceCard
                    key={peer.device_id}
                    peer={peer}
                    onClick={(p) => navigate({ to: '/device/$id', params: { id: p.device_id } })}
                  />
                ))}
              </div>
            )}
          </div>

          {/* Pending connection requests */}
          {pending.length > 1 && (
            <>
              <div className="section" style={{ marginTop: 24 }}>
                <div className="section-header">
                  <span className="section-title">Pending Connections</span>
                </div>
                <div className="device-list">
                  {pending.slice(1).map((req) => {
                    const logo = getBrand(req.name);
                    return (
                      <div key={req.deviceId} className="device-card">
                        <div className="device-card-header">
                          <div className="device-icon-wrap" style={{ height: 28 }}>
                            {logo}
                          </div>
                        </div>
                        <div className="device-info">
                          <div className="device-name">{req.name}</div>
                          <div className="device-meta">Waiting for verification…</div>
                        </div>
                        <div className="device-card-footer">
                          <div className="badge badge-connecting" style={{ background: 'transparent', border: 'none', padding: 0 }}>
                            <span style={{ width: 6, height: 6, borderRadius: '50%', background: 'var(--warning)', display: 'inline-block' }} />
                            <span style={{ color: 'var(--text-secondary)' }}>Pending</span>
                          </div>
                        </div>
                      </div>
                    );
                  })}
                </div>
              </div>
            </>
          )}

        </div>
      </main>

      {currentRequest && (
        <PairModal request={currentRequest} onResolve={resolvePairRequest} />
      )}
    </div>
  )
}
