"use client";

import { useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { purgeConfirmText, type SelectionSummary } from '@/lib/recordings';

interface PurgeConfirmDialogProps {
  open: boolean;
  summary: SelectionSummary;
  onCancel: () => void;
  onConfirm: () => Promise<void>;
}

export function PurgeConfirmDialog({ open, summary, onCancel, onConfirm }: PurgeConfirmDialogProps) {
  const [busy, setBusy] = useState(false);

  const confirm = async () => {
    setBusy(true);
    try {
      await onConfirm();
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!next && !busy) onCancel(); }}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>Remove audio</DialogTitle>
          <DialogDescription>{purgeConfirmText(summary)}</DialogDescription>
        </DialogHeader>
        <p className="text-sm text-gray-600">
          Transcripts and summaries are kept. The audio cannot be recovered, and a meeting without
          audio can no longer be enhanced.
        </p>
        <DialogFooter>
          <Button variant="outline" onClick={onCancel} disabled={busy}>
            Cancel
          </Button>
          <Button variant="destructive" onClick={confirm} disabled={busy}>
            {busy ? 'Removing...' : 'Remove audio'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
