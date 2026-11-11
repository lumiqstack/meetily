"use client";

import { useEffect, useId, useRef, useState, type FormEvent } from 'react';
import { Loader2, Merge, Pencil } from 'lucide-react';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { displaySpeaker } from '@/lib/speaker-label';
import {
  MeetingSpeaker,
  planSpeakerRename,
  renameScopeText,
} from '@/lib/speaker-names';

export type RenameSpeaker = (oldSpeaker: string, input: string) => Promise<boolean>;

interface SpeakerRenamePopoverProps {
  /** Raw stored speaker value of the segment. */
  speaker: string;
  speakers: readonly MeetingSpeaker[];
  onRename: RenameSpeaker;
}

/** The blue speaker label of a saved-meeting segment, as a button that renames the speaker. */
export function SpeakerRenamePopover({ speaker, speakers, onRename }: SpeakerRenamePopoverProps) {
  const label = displaySpeaker(speaker) ?? speaker;
  const [open, setOpen] = useState(false);

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          aria-label={`Rename speaker ${label}`}
          title="Rename speaker"
          className="group mb-0.5 inline-flex items-center gap-1 rounded-sm text-xs font-semibold text-blue-600 underline-offset-2 hover:underline focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-blue-500 data-[state=open]:underline"
        >
          {label}
          <Pencil
            aria-hidden
            className="h-3 w-3 opacity-0 transition-opacity group-hover:opacity-70 group-focus-visible:opacity-70 group-data-[state=open]:opacity-70"
          />
        </button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-80 p-3" onOpenAutoFocus={(event) => event.preventDefault()}>
        {open && (
          <SpeakerRenameForm
            speaker={speaker}
            speakers={speakers}
            onRename={onRename}
            onDone={() => setOpen(false)}
          />
        )}
      </PopoverContent>
    </Popover>
  );
}

function SpeakerRenameForm({
  speaker,
  speakers,
  onRename,
  onDone,
}: SpeakerRenamePopoverProps & { onDone: () => void }) {
  const label = displaySpeaker(speaker) ?? speaker;
  const [draft, setDraft] = useState(label);
  const [saving, setSaving] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const inputId = useId();

  // Select the current name so typing replaces it.
  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);
  const plan = planSpeakerRename(speaker, draft, speakers.map((s) => s.speaker));
  const segmentCount = speakers.find((s) => s.speaker === speaker)?.segment_count;
  const others = speakers.filter((s) => s.speaker !== speaker);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (saving || plan.kind === 'invalid') return;
    if (plan.kind === 'unchanged') {
      onDone();
      return;
    }
    setSaving(true);
    const ok = await onRename(speaker, draft);
    setSaving(false);
    if (ok) onDone();
  };

  return (
    <form onSubmit={submit} className="space-y-2.5">
      <label className="block text-xs font-medium text-gray-500" htmlFor={inputId}>
        Speaker name
      </label>
      <Input
        id={inputId}
        ref={inputRef}
        value={draft}
        onChange={(event) => setDraft(event.target.value)}
        // Names are not prose: macOS autocorrect would rewrite them on space or Enter.
        autoComplete="off"
        autoCorrect="off"
        autoCapitalize="off"
        spellCheck={false}
        disabled={saving}
        aria-invalid={plan.kind === 'invalid'}
        className="h-8"
      />
      {segmentCount !== undefined && (
        <p className="text-xs text-gray-500">{renameScopeText(segmentCount)}</p>
      )}
      {plan.kind === 'merge' && (
        <p className="flex items-start gap-1.5 rounded-md bg-amber-50 px-2 py-1.5 text-xs text-amber-800">
          <Merge aria-hidden className="mt-0.5 h-3 w-3 shrink-0" />
          Merges with {plan.intoLabel}: both appear as one speaker.
        </p>
      )}
      {plan.kind === 'invalid' && draft.trim() !== '' && (
        <p className="text-xs text-red-600">{plan.reason}</p>
      )}
      {others.length > 0 && (
        <div className="space-y-1">
          <p className="text-xs text-gray-500">Same person as</p>
          <div className="flex flex-wrap gap-1">
            {others.map((other) => {
              const otherLabel = displaySpeaker(other.speaker) ?? other.speaker;
              return (
                <button
                  key={other.speaker}
                  type="button"
                  disabled={saving}
                  onClick={() => {
                    setDraft(otherLabel);
                    inputRef.current?.focus();
                  }}
                  className="max-w-full truncate rounded-full border border-gray-200 bg-gray-50 px-2 py-0.5 text-xs text-gray-700 hover:border-blue-300 hover:bg-blue-50 hover:text-blue-700 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-blue-500"
                >
                  {otherLabel}
                </button>
              );
            })}
          </div>
        </div>
      )}
      <div className="flex justify-end gap-2 pt-1">
        <Button type="button" variant="ghost" size="sm" onClick={onDone} disabled={saving}>
          Cancel
        </Button>
        <Button type="submit" variant="blue" size="sm" disabled={saving || plan.kind === 'invalid'}>
          {saving && <Loader2 className="animate-spin" />}
          {plan.kind === 'merge' ? 'Merge' : 'Save'}
        </Button>
      </div>
    </form>
  );
}
