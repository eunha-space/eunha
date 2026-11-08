import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select.tsx'
import { Switch } from '@/components/ui/switch.tsx'

const DURATIONS = {
  '300': '5 minutes',
  '1800': '30 minutes',
  '3600': '1 hour',
  '21600': '6 hours',
  '86400': '1 day',
  '259200': '3 days',
  '604800': '7 days',
}

export type PollDraft = { options: string[]; expiresIn: number; multiple: boolean }

export function ComposePoll({ poll, onChange, onRemove }: {
  poll: PollDraft
  onChange: (poll: PollDraft) => void
  onRemove: () => void
}) {
  return (
    <fieldset className="space-y-2 rounded-lg border p-3">
      <legend className="px-1 text-sm font-medium">Poll</legend>
      {poll.options.map((option, index) => (
        <div key={index} className="flex gap-2">
          <Input
            aria-label={`Poll option ${index + 1}`}
            placeholder={`Option ${index + 1}`}
            value={option}
            onChange={(event) => onChange({
              ...poll,
              options: poll.options.map((text, i) => i === index ? event.target.value : text),
            })}
          />
          {index >= 2 && (
            <Button variant="ghost" size="sm" aria-label={`Remove option ${index + 1}`}
              onClick={() => onChange({ ...poll, options: poll.options.filter((_, i) => i !== index) })}>
              Remove
            </Button>
          )}
        </div>
      ))}
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" variant="outline" disabled={poll.options.length >= 4}
          onClick={() => onChange({ ...poll, options: [...poll.options, ''] })}>
          Add option
        </Button>
        <Select items={DURATIONS} value={String(poll.expiresIn)} onValueChange={(value) => {
          if (value) onChange({ ...poll, expiresIn: Number(value) })
        }}>
          <SelectTrigger aria-label="Poll duration"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectGroup>
              {Object.entries(DURATIONS).map(([seconds, label]) => (
                <SelectItem key={seconds} value={seconds}>{label}</SelectItem>
              ))}
            </SelectGroup>
          </SelectContent>
        </Select>
        <Label>
          <Switch checked={poll.multiple} onCheckedChange={(multiple) => onChange({ ...poll, multiple })} />
          Allow multiple choices
        </Label>
        <Button size="sm" variant="ghost" onClick={onRemove}>Remove poll</Button>
      </div>
    </fieldset>
  )
}
