import { useUi } from '@/stores/ui';

export function Toasts() {
  const toasts = useUi((s) => s.toasts);
  const dismiss = useUi((s) => s.dismissToast);
  return (
    <div className="rb-toasts" aria-live="polite">
      {toasts.map((t) => (
        <div key={t.id} className={`rb-toast rb-toast--${t.tone}`} role={t.tone === 'critical' ? 'alert' : 'status'} onClick={() => dismiss(t.id)}>
          <span className="rb-toast__dot" aria-hidden />
          <span>{t.message}</span>
        </div>
      ))}
    </div>
  );
}
