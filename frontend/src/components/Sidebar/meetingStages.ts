import type { SidebarItem } from './SidebarProvider';

/** Which pipeline stages a meeting has finished, as reported by `api_get_meetings`. */
export interface MeetingStages {
  transcribed: boolean;
  summarized: boolean;
  /** The meeting has a note in the Obsidian vault. */
  obsidianExported: boolean;
}

export type MeetingStage = keyof MeetingStages;

/** Sidebar filter chips, in display order: each shows meetings missing that stage. */
export const MISSING_STAGE_FILTERS: ReadonlyArray<{ stage: MeetingStage; label: string }> = [
  { stage: 'transcribed', label: 'No transcript' },
  { stage: 'summarized', label: 'No summary' },
  { stage: 'obsidianExported', label: 'Not in Obsidian' },
];

/** Keeps only the meetings that have not finished `stage`; folders stay. */
export function missingStage(items: SidebarItem[], stage: MeetingStage | null): SidebarItem[] {
  if (!stage) return items;
  return items.map(item =>
    item.children ? { ...item, children: item.children.filter(child => !child[stage]) } : item
  );
}
