import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { t } from '@/i18n.tsx'

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
      <legend className="px-1 text-sm font-medium">{t({ id: 'eunha.compose_poll.poll', defaultMessage: 'Poll' })}</legend>
      {poll.options.map((option, index) => (
        <div key={index} className="flex gap-2">
          <Input
            aria-label={t({ id: 'eunha.compose_poll.option_label', defaultMessage: 'Poll option {number}' }, { number: index + 1 })}
            placeholder={t({ id: 'compose_form.poll.option_placeholder', defaultMessage: 'Option {number}' }, { number: index + 1 })}
            value={option}
            onChange={(event) => onChange({
              ...poll,
              options: poll.options.map((text, i) => i === index ? event.target.value : text),
            })}
          />
          {index >= 2 && (
            <Button variant="ghost" size="sm" aria-label={t({ id: 'eunha.compose_poll.remove_option', defaultMessage: 'Remove option {number}' }, { number: index + 1 })}
              onClick={() => onChange({ ...poll, options: poll.options.filter((_, i) => i !== index) })}>
              {t({ id: 'tag.remove', defaultMessage: 'Remove' })}
            </Button>
          )}
        </div>
      ))}
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" variant="outline" disabled={poll.options.length >= 4}
          onClick={() => onChange({ ...poll, options: [...poll.options, ''] })}>
          {t({ id: 'eunha.compose_poll.add_option', defaultMessage: 'Add option' })}
        </Button>
        <Select items={DURATIONS} value={String(poll.expiresIn)} onValueChange={(value) => {
          if (value) onChange({ ...poll, expiresIn: Number(value) })
        }}>
          <SelectTrigger aria-label={t({ id: 'eunha.compose_poll.poll_duration', defaultMessage: 'Poll duration' })}><SelectValue /></SelectTrigger>
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
          {t({ id: 'eunha.compose_poll.allow_multiple_choices', defaultMessage: 'Allow multiple choices' })}
        </Label>
        <Button size="sm" variant="ghost" onClick={onRemove}>{t({ id: 'eunha.compose_poll.remove_poll', defaultMessage: 'Remove poll' })}</Button>
      </div>
    </fieldset>
  )
}
