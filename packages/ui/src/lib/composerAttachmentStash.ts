/**
 * In-memory stash for composer attachments, keyed by the same chat draft
 * identity (runtime, directory, session) that text drafts use.
 *
 * `useInputStore.attachedFiles` is one global list; without scoping, files
 * attached under one session ride into every other session's composer on a
 * switch — the attachment equivalent of a leaked draft. Unlike text drafts
 * the stash is intentionally memory-only: attachments already do not survive
 * a reload, and their data URLs are too large for the persisted draft
 * envelope.
 */

import type { AttachedFile } from '@/stores/types/sessionTypes';
import { getChatDraftIdentityKey, type ChatDraftIdentity } from './chatDraftPersistence';

const stash = new Map<string, AttachedFile[]>();

export const stashComposerAttachments = (
    identity: ChatDraftIdentity | null,
    files: readonly AttachedFile[],
): void => {
    if (!identity) return;
    const key = getChatDraftIdentityKey(identity);
    if (files.length === 0) {
        stash.delete(key);
    } else {
        stash.set(key, [...files]);
    }
};

export const readComposerAttachments = (identity: ChatDraftIdentity | null): AttachedFile[] => {
    if (!identity) return [];
    return [...(stash.get(getChatDraftIdentityKey(identity)) ?? [])];
};

export const clearComposerAttachments = (identity: ChatDraftIdentity | null): void => {
    if (!identity) return;
    stash.delete(getChatDraftIdentityKey(identity));
};
