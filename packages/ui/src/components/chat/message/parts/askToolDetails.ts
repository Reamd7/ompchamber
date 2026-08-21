/**
 * omp ask tool result model (spec 03 §5.4.1): the SDK AskToolDetails object
 * the omp projection carries in the tool part's state.metadata.details
 * (packages/web/server/lib/omp-host/projection.js). Two answer shapes — a
 * flat single-question answer and a multi-question `results` array — plus
 * the "Chat about this" redirect outcome. Anything else parses to null so
 * ToolPart falls through to the generic output rendering.
 */

export interface AskAnswerEntry {
    id: string;
    question: string;
    /** Offered option labels; empty when the details omitted them. */
    options: string[];
    multi: boolean;
    selectedOptions: string[];
    customInput?: string;
    note?: string;
    /** True when the answer was auto-selected because the dialog timed out. */
    timedOut: boolean;
}

export type AskAnswerModel =
    | { kind: 'answers'; entries: AskAnswerEntry[] }
    | { kind: 'chatRedirect'; questions: string[] };

// Canonical guard for this module (no shared UI type-guard module exists).
const isRecord = (value: unknown): value is Record<string, unknown> =>
    typeof value === 'object' && value !== null;

const stringArray = (value: unknown): string[] =>
    Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string') : [];

const parseEntry = (value: unknown, fallbackId: string): AskAnswerEntry | null => {
    if (!isRecord(value)) {
        return null;
    }
    const question = typeof value.question === 'string' ? value.question : '';
    const selectedOptions = stringArray(value.selectedOptions);
    if (!question && selectedOptions.length === 0) {
        return null;
    }
    return {
        id: typeof value.id === 'string' && value.id ? value.id : fallbackId,
        question,
        options: stringArray(value.options),
        multi: value.multi === true,
        selectedOptions,
        customInput: typeof value.customInput === 'string' && value.customInput ? value.customInput : undefined,
        note: typeof value.note === 'string' && value.note ? value.note : undefined,
        timedOut: value.timedOut === true,
    };
};

export const parseAskToolDetails = (value: unknown): AskAnswerModel | null => {
    if (!isRecord(value)) {
        return null;
    }
    if (value.chatRedirect === true) {
        return { kind: 'chatRedirect', questions: stringArray(value.questions) };
    }
    if (Array.isArray(value.results)) {
        const entries = value.results
            .map((item, index) => parseEntry(item, String(index)))
            .filter((entry): entry is AskAnswerEntry => entry !== null);
        return entries.length > 0 ? { kind: 'answers', entries } : null;
    }
    const entry = parseEntry(value, '0');
    return entry ? { kind: 'answers', entries: [entry] } : null;
};
