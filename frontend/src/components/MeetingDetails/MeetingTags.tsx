"use client";

import { useEffect, useId, useRef, useState, KeyboardEvent } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { Tag, X } from 'lucide-react';

interface MeetingTagsProps {
  meetingId: string;
}

/**
 * Tag chips for a meeting. Tags are normalized and stored by the Rust core
 * (`api_set_meeting_tags`), which also mirrors them into the meeting's
 * Obsidian note front matter when it has already been exported.
 */
export function MeetingTags({ meetingId }: MeetingTagsProps) {
  const [tags, setTags] = useState<string[]>([]);
  const [suggestions, setSuggestions] = useState<string[]>([]);
  const [draft, setDraft] = useState('');
  const listId = useId();
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    let cancelled = false;
    setDraft('');
    Promise.all([
      invoke<string[]>('api_get_meeting_tags', { meetingId }),
      invoke<string[]>('api_get_all_meeting_tags'),
    ])
      .then(([meetingTags, allTags]) => {
        if (cancelled) return;
        setTags(meetingTags);
        setSuggestions(allTags);
      })
      .catch((error) => console.error('Failed to load meeting tags:', error));
    return () => {
      cancelled = true;
    };
  }, [meetingId]);

  const save = async (next: string[]) => {
    const previous = tags;
    const wasFocused = typeof document !== 'undefined' && document.activeElement === inputRef.current;
    setTags(next);
    try {
      const stored = await invoke<string[]>('api_set_meeting_tags', { meetingId, tags: next });
      setTags(stored);
      setSuggestions((current) => Array.from(new Set([...current, ...stored])));
    } catch (error) {
      setTags(previous);
      toast.error('Failed to save tags', {
        description: error instanceof Error ? error.message : String(error),
      });
    } finally {
      // Re-rendering the chips and suggestion list drops focus in WebKit;
      // keep the field ready for the next tag if the user was typing in it.
      if (wasFocused) requestAnimationFrame(() => inputRef.current?.focus());
    }
  };

  const addDraft = () => {
    const entries = draft
      .split(',')
      .map((entry) => entry.trim())
      .filter(Boolean);
    setDraft('');
    if (entries.length === 0) return;
    const next = [...tags];
    for (const entry of entries) {
      if (!next.includes(entry.toLowerCase())) next.push(entry);
    }
    if (next.length !== tags.length) void save(next);
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === 'Enter' || event.key === ',') {
      event.preventDefault();
      addDraft();
    } else if (event.key === 'Backspace' && draft === '' && tags.length > 0) {
      void save(tags.slice(0, -1));
    }
  };

  return (
    <div
      className="flex items-center flex-wrap gap-1.5 px-4 py-2 border-b border-gray-200 text-sm"
      onClick={() => inputRef.current?.focus()}
    >
      <Tag className="w-4 h-4 text-gray-400 flex-shrink-0" aria-hidden />
      {tags.map((tag) => (
        <span
          key={tag}
          className="inline-flex items-center gap-1 rounded-full bg-purple-50 text-purple-700 px-2 py-0.5"
        >
          {tag}
          <button
            type="button"
            onClick={(event) => {
              event.stopPropagation();
              void save(tags.filter((t) => t !== tag));
            }}
            className="text-purple-400 hover:text-purple-700"
            aria-label={`Remove tag ${tag}`}
          >
            <X className="w-3 h-3" />
          </button>
        </span>
      ))}
      <input
        ref={inputRef}
        value={draft}
        onChange={(event) => setDraft(event.target.value)}
        onKeyDown={handleKeyDown}
        onBlur={addDraft}
        list={listId}
        placeholder={tags.length === 0 ? 'Add tags…' : ''}
        aria-label="Add tag"
        className="flex-1 min-w-[6rem] bg-transparent outline-none placeholder:text-gray-400"
      />
      <datalist id={listId}>
        {suggestions
          .filter((s) => !tags.includes(s))
          .map((s) => (
            <option key={s} value={s} />
          ))}
      </datalist>
    </div>
  );
}
