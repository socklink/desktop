import { Smartphone } from 'lucide-react'
import { SiSamsung, SiGoogle, SiOneplus, SiXiaomi, SiMotorola, SiHuawei, SiOppo, SiVivo, SiLg, SiSony, SiNokia, SiApple } from 'react-icons/si'

export type PairRequest = {
  deviceId: string
  name: string
  code: string
  ip: string
}

export type TrustedPeer = {
  device_id: string
  name: string
  last_seen: number
  connected: boolean
  clipboard_sync_enabled: boolean
  media_controls_enabled: boolean
  volume_sync_enabled: boolean
  incoming_files_enabled: boolean
  terminal_access_enabled: boolean
  audio_streaming_enabled: boolean
}

export function getBrand(name: string) {
  const n = name.toLowerCase()
  const iconStyle = { height: '100%', width: 'auto' }
  if (n.includes('samsung') || n.includes('galaxy') || n.includes('sm-')) return <SiSamsung style={iconStyle} />
  if (n.includes('pixel') || n.includes('google')) return <SiGoogle style={iconStyle} />
  if (n.includes('oneplus')) return <SiOneplus style={iconStyle} />
  if (n.includes('xiaomi') || n.includes('redmi') || n.includes('poco') || n.includes('mi ')) return <SiXiaomi style={iconStyle} />
  if (n.includes('motorola') || n.includes('moto')) return <SiMotorola style={iconStyle} />
  if (n.includes('huawei') || n.includes('honor')) return <SiHuawei style={iconStyle} />
  if (n.includes('oppo')) return <SiOppo style={iconStyle} />
  if (n.includes('vivo')) return <SiVivo style={iconStyle} />
  if (n.includes('realme')) return <Smartphone style={iconStyle} strokeWidth={2} />
  if (n.includes('sony') || n.includes('xperia')) return <SiSony style={iconStyle} />
  if (n.includes('nokia')) return <SiNokia style={iconStyle} />
  if (n.includes('lg ')) return <SiLg style={iconStyle} />
  if (n.includes('iphone') || n.includes('ipad') || n.includes('apple') || n.includes('macbook') || n.includes('mac ')) return <SiApple style={iconStyle} />
  return <Smartphone style={iconStyle} strokeWidth={2} />
}

export function relativeTime(unixSecs: number): string {
  const diff = Math.floor(Date.now() / 1000) - unixSecs
  if (diff < 60) return 'just now'
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`
  return `${Math.floor(diff / 86400)}d ago`
}
