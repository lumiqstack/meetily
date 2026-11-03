import { beforeEach, describe, expect, test } from 'bun:test';
import {
  ARM_DURATION_MS,
  armJoinAndRecord,
  setActiveCalendarEvent,
  stashCalendarEventForAutoStart,
  startPromptTitle,
  takeActiveCalendarEvent,
  takeArmedJoin,
  takeCalendarEventForAutoStart,
} from '@/lib/calendar-handoff';
import type { CalendarEvent } from '@/types/calendar';

const event: CalendarEvent = {
  id: 'uid@2026-11-02T10:00:00+00:00',
  source: 'outlook_com',
  title: 'Weekly sync',
  start: '2026-11-02T10:00:00Z',
  end: '2026-11-02T10:30:00Z',
  organizer: 'Ana',
  attendees: ['Ana', 'Bo'],
  location: 'Microsoft Teams Meeting',
  body: 'Agenda',
  teams_join_url: 'https://teams.microsoft.com/l/meetup-join/x',
  is_recurring_instance: true,
};

class MemoryStorage implements Storage {
  private map = new Map<string, string>();
  get length() { return this.map.size; }
  clear() { this.map.clear(); }
  getItem(key: string) { return this.map.get(key) ?? null; }
  key(index: number) { return Array.from(this.map.keys())[index] ?? null; }
  removeItem(key: string) { this.map.delete(key); }
  setItem(key: string, value: string) { this.map.set(key, String(value)); }
}

beforeEach(() => {
  const storage = new MemoryStorage();
  (globalThis as unknown as { window: unknown }).window = { sessionStorage: storage };
});

describe('auto-start handoff', () => {
  test('stash sets the title and the auto-start flag, take clears the event', () => {
    stashCalendarEventForAutoStart(event);
    const store = window.sessionStorage;
    expect(store.getItem('autoStartMeetingName')).toBe('Weekly sync');
    expect(store.getItem('autoStartRecording')).toBe('true');

    expect(takeCalendarEventForAutoStart()).toEqual(event);
    expect(takeCalendarEventForAutoStart()).toBeNull();
  });

  test('active event is consumed by one save and a manual start clears it', () => {
    setActiveCalendarEvent(event);
    expect(takeActiveCalendarEvent()).toEqual(event);
    expect(takeActiveCalendarEvent()).toBeNull();

    setActiveCalendarEvent(event);
    setActiveCalendarEvent(null);
    expect(takeActiveCalendarEvent()).toBeNull();
  });
});

describe('join and record arming', () => {
  test('armed event is returned once within the window', () => {
    armJoinAndRecord(event, 1_000);
    expect(takeArmedJoin(1_000 + ARM_DURATION_MS)).toEqual(event);
    expect(takeArmedJoin(1_000 + ARM_DURATION_MS)).toBeNull();
  });

  test('an expired arm is dropped', () => {
    armJoinAndRecord(event, 1_000);
    expect(takeArmedJoin(1_000 + ARM_DURATION_MS + 1)).toBeNull();
  });
});

describe('startPromptTitle', () => {
  test('calendar title wins over the window title', () => {
    expect(startPromptTitle('Weekly sync | window', event)).toBe('Weekly sync');
  });
  test('window title is used without a match', () => {
    expect(startPromptTitle('  Design review ', null)).toBe('Design review');
  });
  test('nothing known yields null', () => {
    expect(startPromptTitle('  ', null)).toBeNull();
    expect(startPromptTitle(null, { ...event, title: '' })).toBeNull();
  });
});
