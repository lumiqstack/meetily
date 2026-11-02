/**
 * Carries an Outlook calendar event from the prompt that offered it to the
 * recording that gets saved, through sessionStorage so it survives the
 * navigation to `/` that every auto-start does.
 *
 *   prompt click ──stash──▶ autoStart path ──activate──▶ save ──take──▶ api_save_transcript
 *
 * "Join and record" adds an arming step: the event waits until the Teams
 * call detector fires, then the start happens without a second prompt.
 */
import type { CalendarEvent } from '@/types/calendar';

const AUTO_START_EVENT_KEY = 'autoStartCalendarEvent';
const ACTIVE_EVENT_KEY = 'activeCalendarEvent';
const ARMED_KEY = 'teamsJoinArmed';

/** How long "Join and record" waits for the call to be detected. */
export const ARM_DURATION_MS = 10 * 60 * 1000;

function storage(): Storage | null {
  try {
    return typeof window === 'undefined' ? null : window.sessionStorage;
  } catch {
    return null;
  }
}

function readJson<T>(key: string): T | null {
  const raw = storage()?.getItem(key);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as T;
  } catch {
    return null;
  }
}

/** Hand an event to the next auto-start, together with its title. */
export function stashCalendarEventForAutoStart(event: CalendarEvent): void {
  const store = storage();
  if (!store) return;
  store.setItem(AUTO_START_EVENT_KEY, JSON.stringify(event));
  store.setItem('autoStartMeetingName', event.title);
  store.setItem('autoStartRecording', 'true');
}

/** The event stashed for this auto-start, cleared on read. */
export function takeCalendarEventForAutoStart(): CalendarEvent | null {
  const event = readJson<CalendarEvent>(AUTO_START_EVENT_KEY);
  storage()?.removeItem(AUTO_START_EVENT_KEY);
  return event;
}

/**
 * Mark the event the running recording belongs to. Every start path calls
 * this, with `null` for a manual start, so a stale event from an abandoned
 * prompt never attaches to an unrelated recording.
 */
export function setActiveCalendarEvent(event: CalendarEvent | null): void {
  const store = storage();
  if (!store) return;
  if (event) {
    store.setItem(ACTIVE_EVENT_KEY, JSON.stringify(event));
  } else {
    store.removeItem(ACTIVE_EVENT_KEY);
  }
}

/** The running recording's event, cleared on read (the save consumes it). */
export function takeActiveCalendarEvent(): CalendarEvent | null {
  const event = readJson<CalendarEvent>(ACTIVE_EVENT_KEY);
  storage()?.removeItem(ACTIVE_EVENT_KEY);
  return event;
}

interface ArmedJoin {
  event: CalendarEvent;
  expiresAt: number;
}

/** After "Join and record": the next detected Teams call records this event. */
export function armJoinAndRecord(event: CalendarEvent, now = Date.now()): void {
  const armed: ArmedJoin = { event, expiresAt: now + ARM_DURATION_MS };
  storage()?.setItem(ARMED_KEY, JSON.stringify(armed));
}

/** The armed event if still within its window, cleared on read either way. */
export function takeArmedJoin(now = Date.now()): CalendarEvent | null {
  const armed = readJson<ArmedJoin>(ARMED_KEY);
  storage()?.removeItem(ARMED_KEY);
  if (!armed || typeof armed.expiresAt !== 'number' || now > armed.expiresAt) return null;
  return armed.event;
}

export function disarmJoinAndRecord(): void {
  storage()?.removeItem(ARMED_KEY);
}

/**
 * Title for the Teams start prompt and the recording it starts:
 * calendar match, then the Teams window subject, then nothing (the caller
 * falls back to its generic title).
 */
export function startPromptTitle(
  windowTitle: string | null | undefined,
  calendarEvent: CalendarEvent | null | undefined,
): string | null {
  const fromCalendar = calendarEvent?.title?.trim();
  if (fromCalendar) return fromCalendar;
  const fromWindow = windowTitle?.trim();
  return fromWindow || null;
}
