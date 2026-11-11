"use client";

import { useState, type FormEvent } from 'react';
import { Loader2 } from 'lucide-react';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { displaySpeaker } from '@/lib/speaker-label';
import { MeetingSpeaker, planSpeakerRename } from '@/lib/speaker-names';
import type { RenameSpeaker } from './SpeakerRenamePopover';

interface SpeakersDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  speakers: readonly MeetingSpeaker[];
  onRename: RenameSpeaker;
}

function formatStart(seconds: number | null): string | null {
  if (seconds === null) return null;
  const total = Math.max(0, Math.floor(seconds));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = String(total % 60).padStart(2, '0');
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${s}` : `${m}:${s}`;
}

/** Names every speaker of a saved meeting in one place. */
export function SpeakersDialog({ open, onOpenChange, speakers, onRename }: SpeakersDialogProps) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>Speakers</DialogTitle>
          <DialogDescription>
            Name each speaker. A new name applies to all of their segments in this meeting; using
            another speaker&apos;s name merges the two.
          </DialogDescription>
        </DialogHeader>
        <ul className="max-h-[60vh] space-y-3 overflow-y-auto pr-1">
          {speakers.map((row) => (
            <SpeakerRow key={row.speaker} row={row} speakers={speakers} onRename={onRename} />
          ))}
        </ul>
      </DialogContent>
    </Dialog>
  );
}

function SpeakerRow({
  row,
  speakers,
  onRename,
}: {
  row: MeetingSpeaker;
  speakers: readonly MeetingSpeaker[];
  onRename: RenameSpeaker;
}) {
  const label = displaySpeaker(row.speaker) ?? row.speaker;
  const [draft, setDraft] = useState(label);
  const [saving, setSaving] = useState(false);
  const plan = planSpeakerRename(row.speaker, draft, speakers.map((s) => s.speaker));
  const canSave = plan.kind === 'rename' || plan.kind === 'merge';
  const start = formatStart(row.first_start);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!canSave || saving) return;
    setSaving(true);
    // On success the list refreshes and this row remounts under its new key.
    await onRename(row.speaker, draft);
    setSaving(false);
  };

  return (
    <li>
      <form onSubmit={submit} className="space-y-1">
        <div className="flex items-center gap-2">
          <Input
            aria-label={`Name for ${label}`}
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            // Names are not prose: macOS autocorrect would rewrite them on space or Enter.
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            disabled={saving}
            aria-invalid={plan.kind === 'invalid'}
            className="h-8 flex-1"
          />
          <Button
            type="submit"
            size="sm"
            variant={canSave ? 'blue' : 'outline'}
            disabled={!canSave || saving}
            className="w-[4.5rem]"
          >
            {saving ? <Loader2 className="animate-spin" /> : plan.kind === 'merge' ? 'Merge' : 'Save'}
          </Button>
        </div>
        <p className="px-0.5 text-xs text-gray-500">
          {plan.kind === 'merge' ? (
            <span className="text-amber-700">Merges with {plan.intoLabel}</span>
          ) : plan.kind === 'invalid' ? (
            <span className="text-red-600">{plan.reason}</span>
          ) : (
            <>
              {label !== draft.trim() ? `Was ${label} · ` : ''}
              {row.segment_count === 1 ? '1 segment' : `${row.segment_count} segments`}
              {start && ` · first speaks at ${start}`}
            </>
          )}
        </p>
      </form>
    </li>
  );
}
