import { Check, X } from 'lucide-react'
import { PairRequest } from '../../utils/device'

interface PairModalProps {
  request: PairRequest
  onResolve: (id: string, accept: boolean) => void
}

export function PairModal({ request, onResolve }: PairModalProps) {
  return (
    <div className="modal-backdrop">
      <div className="modal-card">
        <div className="modal-title">{request.name}</div>
        <div className="modal-sub">
          Confirm the code below matches what's shown on the Android device before accepting.
          Only accept if you started this pairing yourself.
        </div>
        <div className="modal-sub" style={{ fontFamily: 'var(--font-mono, monospace)' }}>
          From {request.ip}
        </div>
        <div className="modal-code" style={{ textShadow: 'none', color: 'var(--text-primary)' }}>
          {request.code.slice(0, 3)} {request.code.slice(3)}
        </div>
        <div className="modal-actions">
          <button
            className="btn btn-ghost"
            onClick={() => onResolve(request.deviceId, false)}
          >
            <X size={16} strokeWidth={2.5} /> Reject
          </button>
          <button
            className="btn btn-primary"
            style={{ background: 'var(--text-primary)', color: 'var(--bg-base)', boxShadow: 'none' }}
            onClick={() => onResolve(request.deviceId, true)}
          >
            <Check size={16} strokeWidth={2.5} /> Accept
          </button>
        </div>
      </div>
    </div>
  )
}
