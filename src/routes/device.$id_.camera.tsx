import { createFileRoute, useNavigate } from '@tanstack/react-router'
import { useState, useEffect } from 'react'
import { listen } from '@tauri-apps/api/event'
import { invoke } from '@tauri-apps/api/core'
import {
  ChevronLeft,
  Camera,
  Video,
  AlertCircle,
  CheckCircle2,
  Smartphone,
  Sliders,
  Tv,
  Gauge,
  RotateCw,
  Wifi,
  Lock
} from 'lucide-react'
import { Toggle } from '../components/ui/toggle'
import { useDeviceSettings } from '../hooks/use-device-settings'
import { DependencyModal, MissingDependency } from '../components/layout/dependency-modal'

export const Route = createFileRoute('/device/$id_/camera')({
  component: DeviceCameraRoute,
})

function DeviceCameraRoute() {
  const { id } = Route.useParams()
  const navigate = useNavigate()
  const { peer } = useDeviceSettings(id)

  const [missingDependency, setMissingDependency] = useState<MissingDependency | null>(null)
  const [cameraStreaming, setCameraStreaming] = useState(false)
  const [cameraIp, setCameraIp] = useState('')
  const [cameraPort, setCameraPort] = useState(0)
  const [cameraError, setCameraError] = useState<string | null>(null)
  const [previewError, setPreviewError] = useState<string | null>(null)
  const [frameSrc, setFrameSrc] = useState<string | null>(null)
  const [, setErrorCount] = useState(0)
  const [_virtualCameraActive, setVirtualCameraActive] = useState(false)
  const [virtualCameraError, setVirtualCameraError] = useState<string | null>(null)

  const [cameraConfig, setCameraConfig] = useState<{
    isFront: boolean
    resolution: string
    fps: number
    rotation: number
    useAdb: boolean
  }>({
    isFront: false,
    resolution: '1920x1080',
    fps: 30,
    rotation: 0,
    useAdb: false,
  })

  useEffect(() => {
    invoke('request_camera_config', { deviceId: id }).catch(() => {})
    invoke<boolean>('get_camera_stream_state').then((active) => setCameraStreaming(active)).catch(() => {})

    // Listen for camera stream changes
    const unlistenCamera = listen<{ streaming: boolean; ip?: string; port?: number; use_adb?: boolean }>(
      'camera-stream-state-changed',
      (event) => {
        const payload = event.payload
        setCameraStreaming(payload.streaming)
        if (payload.streaming && payload.ip && payload.port) {
          const useAdb = payload.use_adb ?? false
          setCameraIp(useAdb ? '127.0.0.1' : payload.ip)
          setCameraPort(useAdb ? 40000 : payload.port)
          setCameraError(null)
          setPreviewError(null)
          setErrorCount(0)
        } else {
          setCameraIp('')
          setCameraPort(0)
          setCameraError(null)
          setPreviewError(null)
          setFrameSrc(null)
          setErrorCount(0)
        }
      }
    )

    // Listen for camera config updates from phone
    const unlistenCameraConfig = listen<{
      deviceId: string
      is_front: boolean
      resolution: string
      fps: number
      rotation: number
      use_adb: boolean
    }>('camera-config-changed', (event) => {
      if (event.payload.deviceId === id) {
        setCameraConfig({
          isFront: event.payload.is_front,
          resolution: event.payload.resolution,
          fps: event.payload.fps,
          rotation: event.payload.rotation,
          useAdb: event.payload.use_adb,
        })
      }
    })

    // Listen for virtual camera state changes
    const unlistenVirtualCamera = listen<{ active: boolean; error: string | null }>(
      'virtual-camera-state-changed',
      (event) => {
        setVirtualCameraActive(event.payload.active)
        setVirtualCameraError(event.payload.error)
      }
    )

    return () => {
      unlistenCamera.then((fn) => fn()).catch(() => {})
      unlistenCameraConfig.then((fn) => fn()).catch(() => {})
      unlistenVirtualCamera.then((fn) => fn()).catch(() => {})
    }
  }, [id])

  const updateCameraConfig = (patch: Partial<{
    isFront: boolean
    resolution: string
    fps: number
    rotation: number
    useAdb: boolean
  }>) => {
    if (cameraStreaming) return // Locked while streaming

    const next = { ...cameraConfig, ...patch }
    setCameraConfig(next)
    invoke('update_camera_config', {
      deviceId: id,
      isFront: patch.isFront !== undefined ? patch.isFront : null,
      resolution: patch.resolution !== undefined ? patch.resolution : null,
      fps: patch.fps !== undefined ? patch.fps : null,
      rotation: patch.rotation !== undefined ? patch.rotation : null,
      useAdb: patch.useAdb !== undefined ? patch.useAdb : null,
    }).catch((e) => {
      console.error('Failed to update camera config:', e)
    })
  }

  const toggleCameraStream = async () => {
    const nextState = !cameraStreaming
    setCameraError(null)
    setPreviewError(null)
    setFrameSrc(null)
    setErrorCount(0)
    setVirtualCameraActive(false)
    setVirtualCameraError(null)
    if (nextState) {
      const missing = await invoke<MissingDependency | null>('check_system_deps', { feature: 'camera' }).catch(() => null)
      if (missing) {
        setMissingDependency(missing)
        return
      }
    } else {
      setCameraStreaming(false)
    }
    await invoke('toggle_camera_stream', { deviceId: id, start: nextState }).catch((e) => {
      console.error('Failed to toggle camera stream:', e)
      if (nextState) {
        setCameraStreaming(false)
      }
    })
  }

  useEffect(() => {
    if (!cameraStreaming || !cameraIp || !cameraPort) {
      setFrameSrc(null)
      return
    }

    const loadStream = async () => {
      try {
        // The phone's stream needs a secret that only the desktop backend holds, so the
        // preview always comes from the local relay.
        setFrameSrc(`http://127.0.0.1:40001/?t=${Date.now()}`)
      } catch (err) {
        console.error('Failed to initialize stream feed:', err)
        setPreviewError('Failed to initialize camera preview feed.')
      }
    }

    const t = setTimeout(loadStream, 400)
    return () => clearTimeout(t)
  }, [cameraStreaming, cameraIp, cameraPort, cameraConfig.useAdb])

  return (
    <div style={{
      width: '100vw',
      height: '100vh',
      display: 'flex',
      flexDirection: 'column',
      background: 'var(--bg-base)',
      color: 'var(--text-primary)',
      overflow: 'hidden'
    }}>
      {missingDependency && (
        <DependencyModal
          dependency={missingDependency}
          onClose={() => setMissingDependency(null)}
          onSuccess={() => {
            setMissingDependency(null)
            toggleCameraStream()
          }}
        />
      )}

      {/* Top Header Bar */}
      <header style={{
        height: '56px',
        borderBottom: '1px solid var(--border)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        padding: '0 16px',
        background: 'color-mix(in srgb, var(--bg-surface) 80%, transparent)',
        backdropFilter: 'blur(16px)',
        WebkitBackdropFilter: 'blur(16px)',
        flexShrink: 0,
        zIndex: 20
      }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '10px', minWidth: 0 }}>
          <button
            onClick={() => navigate({ to: '/device/$id', params: { id } })}
            className="btn btn-ghost"
            style={{
              padding: '6px',
              borderRadius: '8px',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
              color: 'var(--text-secondary)',
              background: 'var(--bg-elevated)',
              border: '1px solid var(--border)',
              flexShrink: 0
            }}
            title="Back to Device Settings"
          >
            <ChevronLeft size={18} />
          </button>
          <div style={{ display: 'flex', alignItems: 'center', gap: '8px', minWidth: 0 }}>
            <div style={{
              width: 28,
              height: 28,
              borderRadius: '7px',
              background: 'rgba(59, 130, 246, 0.12)',
              color: 'var(--accent)',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
              flexShrink: 0
            }}>
              <Camera size={15} />
            </div>
            <div style={{ minWidth: 0 }}>
              <div style={{
                fontSize: '14px',
                fontWeight: 650,
                color: 'var(--text-primary)',
                whiteSpace: 'nowrap',
                overflow: 'hidden',
                textOverflow: 'ellipsis',
                lineHeight: 1.2
              }}>
                Phone Camera
              </div>
              <div style={{
                fontSize: '11px',
                color: 'var(--text-tertiary)',
                display: 'flex',
                alignItems: 'center',
                gap: '4px',
                whiteSpace: 'nowrap',
                overflow: 'hidden',
                textOverflow: 'ellipsis',
                lineHeight: 1.2,
                marginTop: '1px'
              }}>
                <Smartphone size={10} style={{ flexShrink: 0 }} />
                <span>{peer?.name || 'Device'}</span>
              </div>
            </div>
          </div>
        </div>

        {/* Live Status Pill */}
        <div style={{ flexShrink: 0, marginLeft: '8px' }}>
          {cameraStreaming ? (
            <div style={{
              fontSize: '11px',
              color: '#10b981',
              background: 'rgba(16, 185, 129, 0.12)',
              border: '1px solid rgba(16, 185, 129, 0.3)',
              padding: '3px 9px',
              borderRadius: '20px',
              fontWeight: 650,
              display: 'flex',
              alignItems: 'center',
              gap: '6px',
              letterSpacing: '0.03em'
            }}>
              <span style={{
                width: 6,
                height: 6,
                borderRadius: '50%',
                background: '#10b981',
                boxShadow: '0 0 8px #10b981',
                display: 'inline-block'
              }} />
              LIVE
            </div>
          ) : (
            <div style={{
              fontSize: '11px',
              color: 'var(--text-tertiary)',
              background: 'var(--bg-elevated)',
              border: '1px solid var(--border)',
              padding: '3px 8px',
              borderRadius: '20px',
              fontWeight: 500,
              display: 'flex',
              alignItems: 'center',
              gap: '5px'
            }}>
              <span style={{ width: 5, height: 5, borderRadius: '50%', background: 'var(--text-tertiary)', display: 'inline-block' }} />
              IDLE
            </div>
          )}
        </div>
      </header>

      {/* Main Scrollable View */}
      <div style={{
        flex: 1,
        overflowY: 'auto',
        padding: '18px 16px',
        display: 'flex',
        flexDirection: 'column',
        gap: '18px',
        maxWidth: '720px',
        margin: '0 auto',
        width: '100%',
        boxSizing: 'border-box'
      }}>
        {/* Master Control Card */}
        <div style={{
          background: 'var(--bg-surface)',
          border: '1px solid var(--border)',
          borderRadius: 'var(--radius-lg)',
          padding: '16px 18px',
          boxShadow: '0 4px 20px rgba(0,0,0,0.1)',
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          gap: '14px'
        }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '12px', minWidth: 0 }}>
            <div style={{
              width: 40,
              height: 40,
              borderRadius: '10px',
              background: cameraStreaming ? 'rgba(59, 130, 246, 0.16)' : 'var(--bg-elevated)',
              border: cameraStreaming ? '1px solid rgba(59, 130, 246, 0.3)' : '1px solid var(--border)',
              color: cameraStreaming ? 'var(--accent)' : 'var(--text-tertiary)',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
              flexShrink: 0,
              transition: 'all 0.2s ease'
            }}>
              <Video size={20} />
            </div>
            <div style={{ minWidth: 0 }}>
              <div style={{ fontSize: '14.5px', fontWeight: 600, color: 'var(--text-primary)', lineHeight: 1.3 }}>
                Camera Streaming
              </div>
              <div style={{ fontSize: '12px', color: 'var(--text-tertiary)', marginTop: '2px', lineHeight: 1.3 }}>
                {cameraStreaming
                  ? 'Exposing video feed as system virtual webcam'
                  : 'Turn on to stream phone video into PC meeting apps'}
              </div>
            </div>
          </div>
          <div style={{ flexShrink: 0 }}>
            <Toggle enabled={cameraStreaming} onToggle={toggleCameraStream} id="toggle-camera-main" />
          </div>
        </div>

        {/* Live Stream Preview (when active) */}
        {cameraStreaming && cameraIp && cameraPort && (
          <div style={{ display: 'flex', flexDirection: 'column', gap: '10px' }}>
            <div style={{
              background: 'var(--bg-surface)',
              border: '1px solid var(--border)',
              borderRadius: 'var(--radius-lg)',
              overflow: 'hidden',
              position: 'relative',
              aspectRatio: '16/9',
              boxShadow: '0 8px 32px rgba(0,0,0,0.3)'
            }}>
              {cameraError || previewError ? (
                <div style={{
                  position: 'absolute',
                  inset: 0,
                  display: 'flex',
                  flexDirection: 'column',
                  alignItems: 'center',
                  justifyContent: 'center',
                  padding: '20px',
                  background: 'rgba(20, 20, 22, 0.95)',
                  color: 'var(--text-primary)',
                  textAlign: 'center'
                }}>
                  <AlertCircle size={28} color="var(--danger)" style={{ marginBottom: '8px' }} />
                  <div style={{ color: 'var(--danger)', fontSize: '14px', fontWeight: 600, marginBottom: '4px' }}>
                    Stream Connection Error
                  </div>
                  <div style={{ fontSize: '12px', color: 'var(--text-secondary)', maxWidth: '90%' }}>
                    {cameraError || previewError}
                  </div>
                </div>
              ) : frameSrc ? (
                <img
                  src={frameSrc}
                  alt="Camera stream"
                  onError={() => {
                    setErrorCount(c => {
                      const nextCount = c + 1
                      if (nextCount < 15) {
                        setTimeout(() => {
                          if (cameraStreaming) {
                            setFrameSrc(`http://127.0.0.1:40001/?t=${Date.now()}`)
                          }
                        }, 500)
                      } else {
                        setPreviewError("The image preview failed to load. The loopback address may be blocked or unreachable.")
                      }
                      return nextCount
                    })
                  }}
                  onLoad={() => setErrorCount(0)}
                  style={{
                    width: '100%',
                    height: '100%',
                    objectFit: 'contain',
                    background: '#000'
                  }}
                />
              ) : (
                <div style={{
                  position: 'absolute',
                  inset: 0,
                  display: 'flex',
                  flexDirection: 'column',
                  alignItems: 'center',
                  justifyContent: 'center',
                  background: 'rgba(20, 20, 22, 0.6)',
                  gap: '8px'
                }}>
                  <div className="progress-spinner" style={{ width: 24, height: 24 }} />
                  <span style={{ fontSize: '11.5px', color: 'var(--text-tertiary)' }}>Connecting to camera stream...</span>
                </div>
              )}

              <div style={{
                position: 'absolute',
                top: 10,
                left: 10,
                background: 'rgba(0, 0, 0, 0.75)',
                backdropFilter: 'blur(8px)',
                WebkitBackdropFilter: 'blur(8px)',
                padding: '3px 8px',
                borderRadius: '6px',
                fontSize: '10.5px',
                fontWeight: 600,
                color: '#fff',
                display: 'flex',
                alignItems: 'center',
                gap: '5px',
                border: '1px solid rgba(255, 255, 255, 0.12)'
              }}>
                <span style={{
                  width: 6,
                  height: 6,
                  borderRadius: '50%',
                  background: '#10b981',
                  boxShadow: '0 0 6px #10b981',
                  display: 'inline-block'
                }} />
                {cameraConfig.resolution} • {cameraConfig.fps} FPS
              </div>
            </div>

            {/* Virtual Camera Meeting Status */}
            <div style={{
              background: 'rgba(16, 185, 129, 0.08)',
              border: '1px solid rgba(16, 185, 129, 0.2)',
              borderRadius: 'var(--radius-md)',
              padding: '10px 14px',
              fontSize: '12px',
              color: 'var(--text-secondary)',
              display: 'flex',
              alignItems: 'center',
              gap: '10px'
            }}>
              <CheckCircle2 size={16} color="#10b981" style={{ flexShrink: 0 }} />
              <span>Available in Zoom, Google Meet, Teams, and Discord as <strong style={{ color: '#10b981' }}>Sync Camera</strong></span>
            </div>

            {virtualCameraError && (
              <div style={{
                background: 'rgba(239, 68, 68, 0.1)',
                border: '1px solid rgba(239, 68, 68, 0.25)',
                borderRadius: 'var(--radius-md)',
                padding: '10px 14px',
                fontSize: '12px',
                color: 'var(--danger)',
                display: 'flex',
                alignItems: 'center',
                gap: '10px'
              }}>
                <AlertCircle size={16} color="var(--danger)" style={{ flexShrink: 0 }} />
                <span>{virtualCameraError}</span>
              </div>
            )}
          </div>
        )}

        {/* Camera Configuration Section */}
        <div style={{
          background: 'var(--bg-surface)',
          border: '1px solid var(--border)',
          borderRadius: 'var(--radius-lg)',
          padding: '18px',
          boxShadow: '0 4px 20px rgba(0,0,0,0.06)',
          display: 'flex',
          flexDirection: 'column',
          gap: '14px'
        }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
            <div style={{
              width: 30,
              height: 30,
              borderRadius: '7px',
              background: 'rgba(59, 130, 246, 0.12)',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
              color: 'var(--accent)',
              flexShrink: 0
            }}>
              <Sliders size={16} />
            </div>
            <div>
              <div style={{ fontSize: '14px', fontWeight: 650, color: 'var(--text-primary)' }}>
                Camera Options
              </div>
              <div style={{ fontSize: '11.5px', color: 'var(--text-tertiary)' }}>
                Hardware lens, resolution, frame rate, and orientation
              </div>
            </div>
          </div>

          {/* Notice Banner When Active */}
          {cameraStreaming && (
            <div style={{
              background: 'rgba(234, 179, 8, 0.08)',
              border: '1px solid rgba(234, 179, 8, 0.25)',
              borderRadius: 'var(--radius-md)',
              padding: '10px 12px',
              display: 'flex',
              alignItems: 'center',
              gap: '10px',
              fontSize: '12px',
              color: '#fbbf24'
            }}>
              <Lock size={15} style={{ flexShrink: 0 }} />
              <span>
                <strong>Camera stream is active.</strong> Stop streaming to modify camera options.
              </span>
            </div>
          )}

          {/* Options Grid */}
          <div style={{
            display: 'grid',
            gridTemplateColumns: 'repeat(auto-fit, minmax(190px, 1fr))',
            gap: '12px',
            opacity: cameraStreaming ? 0.5 : 1,
            pointerEvents: cameraStreaming ? 'none' : 'auto',
            transition: 'opacity 0.2s ease'
          }}>
            {/* Camera Facing */}
            <div style={{ display: 'flex', flexDirection: 'column', gap: '5px' }}>
              <label style={{ fontSize: '11.5px', color: 'var(--text-secondary)', fontWeight: 550, display: 'flex', alignItems: 'center', gap: '5px' }}>
                <Camera size={12} style={{ color: 'var(--text-tertiary)' }} />
                <span>Camera Lens</span>
              </label>
              <select
                value={cameraConfig.isFront ? 'front' : 'back'}
                disabled={cameraStreaming}
                onChange={(e) => updateCameraConfig({ isFront: e.target.value === 'front' })}
                style={{
                  background: 'var(--bg-elevated)',
                  color: 'var(--text-primary)',
                  border: '1px solid var(--border)',
                  borderRadius: 'var(--radius-sm)',
                  padding: '7px 10px',
                  fontSize: '12.5px',
                  fontWeight: 500,
                  outline: 'none',
                  cursor: cameraStreaming ? 'not-allowed' : 'pointer',
                  width: '100%'
                }}
              >
                <option value="back">Back Camera (Main Lens)</option>
                <option value="front">Front Camera (Selfie)</option>
              </select>
            </div>

            {/* Resolution */}
            <div style={{ display: 'flex', flexDirection: 'column', gap: '5px' }}>
              <label style={{ fontSize: '11.5px', color: 'var(--text-secondary)', fontWeight: 550, display: 'flex', alignItems: 'center', gap: '5px' }}>
                <Tv size={12} style={{ color: 'var(--text-tertiary)' }} />
                <span>Resolution</span>
              </label>
              <select
                value={cameraConfig.resolution}
                disabled={cameraStreaming}
                onChange={(e) => updateCameraConfig({ resolution: e.target.value })}
                style={{
                  background: 'var(--bg-elevated)',
                  color: 'var(--text-primary)',
                  border: '1px solid var(--border)',
                  borderRadius: 'var(--radius-sm)',
                  padding: '7px 10px',
                  fontSize: '12.5px',
                  fontWeight: 500,
                  outline: 'none',
                  cursor: cameraStreaming ? 'not-allowed' : 'pointer',
                  width: '100%'
                }}
              >
                <option value="1920x1080">1080p Full HD (1920×1080)</option>
                <option value="1280x720">720p HD (1280×720)</option>
                <option value="640x480">480p SD (640×480)</option>
                <option value="3840x2160">4K Ultra HD (3840×2160)</option>
              </select>
            </div>

            {/* Target Frame Rate */}
            <div style={{ display: 'flex', flexDirection: 'column', gap: '5px' }}>
              <label style={{ fontSize: '11.5px', color: 'var(--text-secondary)', fontWeight: 550, display: 'flex', alignItems: 'center', gap: '5px' }}>
                <Gauge size={12} style={{ color: 'var(--text-tertiary)' }} />
                <span>Frame Rate</span>
              </label>
              <select
                value={cameraConfig.fps}
                disabled={cameraStreaming}
                onChange={(e) => updateCameraConfig({ fps: parseInt(e.target.value, 10) })}
                style={{
                  background: 'var(--bg-elevated)',
                  color: 'var(--text-primary)',
                  border: '1px solid var(--border)',
                  borderRadius: 'var(--radius-sm)',
                  padding: '7px 10px',
                  fontSize: '12.5px',
                  fontWeight: 500,
                  outline: 'none',
                  cursor: cameraStreaming ? 'not-allowed' : 'pointer',
                  width: '100%'
                }}
              >
                <option value={60}>60 FPS (Ultra Smooth)</option>
                <option value={30}>30 FPS (Standard)</option>
                <option value={15}>15 FPS (Power Saver)</option>
              </select>
            </div>

            {/* Rotation */}
            <div style={{ display: 'flex', flexDirection: 'column', gap: '5px' }}>
              <label style={{ fontSize: '11.5px', color: 'var(--text-secondary)', fontWeight: 550, display: 'flex', alignItems: 'center', gap: '5px' }}>
                <RotateCw size={12} style={{ color: 'var(--text-tertiary)' }} />
                <span>Orientation</span>
              </label>
              <select
                value={cameraConfig.rotation}
                disabled={cameraStreaming}
                onChange={(e) => updateCameraConfig({ rotation: parseInt(e.target.value, 10) })}
                style={{
                  background: 'var(--bg-elevated)',
                  color: 'var(--text-primary)',
                  border: '1px solid var(--border)',
                  borderRadius: 'var(--radius-sm)',
                  padding: '7px 10px',
                  fontSize: '12.5px',
                  fontWeight: 500,
                  outline: 'none',
                  cursor: cameraStreaming ? 'not-allowed' : 'pointer',
                  width: '100%'
                }}
              >
                <option value={0}>0° (Landscape Default)</option>
                <option value={90}>90° (Portrait Clockwise)</option>
                <option value={180}>180° (Upside Down)</option>
                <option value={270}>270° (Counter-Clockwise)</option>
              </select>
            </div>

            {/* Connection Mode */}
            <div style={{ display: 'flex', flexDirection: 'column', gap: '5px', gridColumn: '1 / -1' }}>
              <label style={{ fontSize: '11.5px', color: 'var(--text-secondary)', fontWeight: 550, display: 'flex', alignItems: 'center', gap: '5px' }}>
                <Wifi size={12} style={{ color: 'var(--text-tertiary)' }} />
                <span>Connection Mode</span>
              </label>
              <select
                value={cameraConfig.useAdb ? 'adb' : 'wifi'}
                disabled={cameraStreaming}
                onChange={(e) => updateCameraConfig({ useAdb: e.target.value === 'adb' })}
                style={{
                  background: 'var(--bg-elevated)',
                  color: 'var(--text-primary)',
                  border: '1px solid var(--border)',
                  borderRadius: 'var(--radius-sm)',
                  padding: '7px 10px',
                  fontSize: '12.5px',
                  fontWeight: 500,
                  outline: 'none',
                  cursor: cameraStreaming ? 'not-allowed' : 'pointer',
                  width: '100%'
                }}
              >
                <option value="wifi">Over Wi-Fi (Wireless streaming over local network)</option>
                <option value="adb">Over USB (High bandwidth, zero-latency ADB cable streaming)</option>
              </select>
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}
