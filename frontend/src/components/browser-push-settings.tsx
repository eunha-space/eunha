import { useEffect, useState } from 'react'
import { toast } from 'sonner'
import { getActiveAccountId } from '../auth.ts'
import {
  browserPushSupported, disableBrowserPush, enableBrowserPush, getBrowserPush,
  pushAlertLabels, setBrowserPushAlert, type BrowserPushState,
} from '@/lib/browser-push.ts'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { Button } from '@/components/ui/button.tsx'
import { errorMessage } from '@/lib/utils.ts'

export function BrowserPushSettings({ token }: { token: string }) {
  const id = getActiveAccountId()
  const [state, setState] = useState<BrowserPushState>()
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    if (!id || !browserPushSupported()) return
    let active = true
    getBrowserPush(id, token).then((value) => {
      if (active) setState(value)
    }).catch(() => {})
    return () => { active = false }
  }, [id, token])

  if (!id || !state?.available) return null

  const run = async (action: () => Promise<void>) => {
    setBusy(true)
    try {
      await action()
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      try {
        setState(await getBrowserPush(id, token))
      } catch {
        toast.error('Could not check browser notifications')
      }
      setBusy(false)
    }
  }

  return (
    <section className="space-y-2 rounded-lg border p-4">
      <h2 className="font-semibold">Browser notifications</h2>
      <p className="text-muted-foreground text-sm">Receive alerts for this account when eunha is closed.</p>
      <Button variant="outline" size="sm" disabled={busy} onClick={() => void run(() =>
        state.enabled ? disableBrowserPush(id, token) : enableBrowserPush(id, token))}>
        {state.enabled ? 'Disable on this browser' : 'Enable on this browser'}
      </Button>
      {state.enabled && (Object.keys(pushAlertLabels) as (keyof typeof pushAlertLabels)[]).map((alert) => (
        <Label key={alert} className="text-sm font-normal">
          <Switch checked={state.alerts[alert] ?? false} disabled={busy}
            onCheckedChange={(on) => void run(() => setBrowserPushAlert(token, state, alert, on))} />
          {pushAlertLabels[alert]}
        </Label>
      ))}
    </section>
  )
}
