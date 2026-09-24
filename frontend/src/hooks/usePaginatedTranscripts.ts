import { useState, useCallback, useRef, useEffect, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Transcript, MeetingMetadata, PaginatedTranscriptsResponse, TranscriptSegmentData } from "@/types";

const DEFAULT_PAGE_SIZE = 100;

export interface RequestStalenessCheck {
    activeMeetingId: string | null;
    requestMeetingId: string | null;
    activeGeneration: number;
    requestGeneration: number;
}

export function isRequestStale({
    activeMeetingId,
    requestMeetingId,
    activeGeneration,
    requestGeneration,
}: RequestStalenessCheck): boolean {
    if (!activeMeetingId || !requestMeetingId) {
        return false;
    }

    return activeMeetingId !== requestMeetingId || activeGeneration !== requestGeneration;
}

interface UsePaginatedTranscriptsProps {
    meetingId: string | null;
    /** Optional initial timestamp (in seconds) from URL for loading the correct page */
    initialTimestamp?: number;
}

interface UsePaginatedTranscriptsReturn {
    metadata: MeetingMetadata | null;
    segments: TranscriptSegmentData[];
    transcripts: Transcript[];
    isLoading: boolean;
    isLoadingMore: boolean;
    hasMore: boolean;
    totalCount: number;
    loadedCount: number;
    error: string | null;

    // Actions
    loadMore: () => Promise<void>;
    reset: () => void;
    refetch: () => Promise<void>;
}

/**
 * Convert Transcript array to TranscriptSegmentData for virtualized display
 */
function convertTranscriptsToSegments(transcripts: Transcript[]): TranscriptSegmentData[] {
    return transcripts.map(t => ({
        id: t.id,
        timestamp: t.audio_start_time ?? 0,
        endTime: t.audio_end_time,
        text: t.text,
        confidence: t.confidence,
        speaker: t.speaker,
    }));
}

export function usePaginatedTranscripts({
    meetingId,
    initialTimestamp,
}: UsePaginatedTranscriptsProps): UsePaginatedTranscriptsReturn {
    const [metadata, setMetadata] = useState<MeetingMetadata | null>(null);
    const [transcripts, setTranscripts] = useState<Transcript[]>([]);
    const [totalCount, setTotalCount] = useState(0);
    const [isLoading, setIsLoading] = useState(true);
    const [isLoadingMore, setIsLoadingMore] = useState(false);
    const [hasMore, setHasMore] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const offsetRef = useRef(0);
    // Transcript ids already loaded; kept across pages so appending one doesn't
    // have to rescan everything loaded so far.
    const loadedIdsRef = useRef<Set<string>>(new Set());
    const loadedMeetingIdRef = useRef<string | null>(null);
    const requestGenerationRef = useRef(0);
    const isLoadingRef = useRef(false);
    const lastLoadTimeRef = useRef(0); // Debounce protection

    // Reset state when meeting changes
    const reset = useCallback(() => {
        requestGenerationRef.current += 1;
        setMetadata(null);
        setTranscripts([]);
        setTotalCount(0);
        setIsLoading(true);
        setIsLoadingMore(false);
        setHasMore(false);
        setError(null);
        offsetRef.current = 0;
        loadedIdsRef.current = new Set();
    }, []);

    // Load meeting metadata
    const loadMetadata = useCallback(async (): Promise<MeetingMetadata | null> => {
        if (!meetingId) return null;

        const requestGeneration = requestGenerationRef.current;

        try {
            const data = await invoke<MeetingMetadata>('api_get_meeting_metadata', {
                meetingId,
            });

            if (isRequestStale({
                activeMeetingId: meetingId,
                requestMeetingId: meetingId,
                activeGeneration: requestGenerationRef.current,
                requestGeneration,
            })) {
                return null;
            }

            setMetadata(data);
            return data;
        } catch (err) {
            if (isRequestStale({
                activeMeetingId: meetingId,
                requestMeetingId: meetingId,
                activeGeneration: requestGenerationRef.current,
                requestGeneration,
            })) {
                return null;
            }

            console.error('Failed to load meeting metadata:', err);
            setError('Failed to load meeting details');
            return null;
        }
    }, [meetingId]);

    // Load transcripts at specific offset
    const loadTranscriptsAtOffset = useCallback(async (
        offset: number,
        append: boolean = true
    ): Promise<Transcript[]> => {
        if (!meetingId) return [];

        const requestGeneration = requestGenerationRef.current;

        try {
            const response = await invoke<PaginatedTranscriptsResponse>(
                'api_get_meeting_transcripts',
                {
                    meetingId,
                    limit: DEFAULT_PAGE_SIZE,
                    offset,
                }
            );

            if (isRequestStale({
                activeMeetingId: meetingId,
                requestMeetingId: meetingId,
                activeGeneration: requestGenerationRef.current,
                requestGeneration,
            })) {
                return [];
            }

            const newTranscripts = response.transcripts;

            if (append) {
                // Deduplicate by id
                const existingIds = loadedIdsRef.current;
                const uniqueNew = newTranscripts.filter(t => !existingIds.has(t.id));
                if (uniqueNew.length > 0) {
                    uniqueNew.forEach(t => existingIds.add(t.id));
                    // Pages arrive in order, so only the new page needs sorting
                    uniqueNew.sort((a, b) => (a.audio_start_time ?? 0) - (b.audio_start_time ?? 0));
                    setTranscripts(prev => {
                        if (isRequestStale({
                            activeMeetingId: meetingId,
                            requestMeetingId: meetingId,
                            activeGeneration: requestGenerationRef.current,
                            requestGeneration,
                        })) {
                            return prev;
                        }
                        return prev.concat(uniqueNew);
                    });
                }
            } else {
                loadedIdsRef.current = new Set(newTranscripts.map(t => t.id));
                setTranscripts(newTranscripts);
            }

            setHasMore(response.has_more);
            setTotalCount(response.total_count);
            offsetRef.current = offset + newTranscripts.length;

            return newTranscripts;
        } catch (err) {
            if (isRequestStale({
                activeMeetingId: meetingId,
                requestMeetingId: meetingId,
                activeGeneration: requestGenerationRef.current,
                requestGeneration,
            })) {
                return [];
            }

            console.error('Failed to load transcripts:', err);
            setError('Failed to load transcripts');
            return [];
        }
    }, [meetingId]);

    // Load next page with debounce protection
    const loadMore = useCallback(async () => {
        const now = Date.now();
        // Debounce: require at least 100ms between calls
        if (now - lastLoadTimeRef.current < 100) {
            return;
        }

        if (isLoadingRef.current || !hasMore || !meetingId || isLoading) return;

        lastLoadTimeRef.current = now;
        isLoadingRef.current = true;
        setIsLoadingMore(true);
        try {
            await loadTranscriptsAtOffset(offsetRef.current, true);
        } finally {
            setIsLoadingMore(false);
            isLoadingRef.current = false;
        }
    }, [hasMore, meetingId, loadTranscriptsAtOffset, isLoading]);

    // Force refetch of data (e.g., after retranscription)
    const refetch = useCallback(async () => {
        if (!meetingId) return;

        requestGenerationRef.current += 1;
        reset();
        setIsLoading(true);
        try {
            await loadMetadata();
            await loadTranscriptsAtOffset(0, false);
        } finally {
            setIsLoading(false);
        }
    }, [meetingId, reset, loadMetadata, loadTranscriptsAtOffset]);

    // Initial load
    useEffect(() => {
        if (!meetingId) {
            requestGenerationRef.current += 1;
            reset();
            return;
        }

        // Avoid reloading the same meeting
        if (loadedMeetingIdRef.current === meetingId) return;
        loadedMeetingIdRef.current = meetingId;
        requestGenerationRef.current += 1;

        reset();

        const loadInitial = async () => {
            setIsLoading(true);
            try {
                await loadMetadata();
                await loadTranscriptsAtOffset(0, false);
            } finally {
                setIsLoading(false);
            }
        };

        loadInitial();
    }, [meetingId, reset, loadMetadata, loadTranscriptsAtOffset]);

    // Convert to segments (memoized)
    const segments = useMemo(() =>
        convertTranscriptsToSegments(transcripts),
        [transcripts]
    );

    return {
        metadata,
        segments,
        transcripts,
        isLoading,
        isLoadingMore,
        hasMore,
        totalCount,
        loadedCount: transcripts.length,
        error,
        loadMore,
        reset,
        refetch,
    };
}
