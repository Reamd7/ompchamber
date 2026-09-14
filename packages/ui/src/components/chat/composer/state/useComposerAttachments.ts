/**
 * Per-session attachment scoping for the composer.
 *
 * `useInputStore.attachedFiles` is a single global list. On a chat draft
 * identity change the outgoing identity's list is stashed and the incoming
 * identity's stash restored — mirroring how `useComposerDraft` treats
 * composer text, so attachments never leak into a session that did not
 * attach them.
 */

import React from 'react';

import { useInputStore } from '@/sync/input-store';
import { getChatDraftIdentityKey, type ChatDraftIdentity } from '@/lib/chatDraftPersistence';
import { readComposerAttachments, stashComposerAttachments } from '@/lib/composerAttachmentStash';

export function useComposerAttachments(identity: ChatDraftIdentity | null): void {
    const previousIdentityRef = React.useRef<ChatDraftIdentity | null>(identity);

    React.useEffect(() => {
        const previous = previousIdentityRef.current;
        const previousKey = previous ? getChatDraftIdentityKey(previous) : null;
        const currentKey = identity ? getChatDraftIdentityKey(identity) : null;
        if (previousKey === currentKey) return;
        previousIdentityRef.current = identity;

        const inputStore = useInputStore.getState();
        stashComposerAttachments(previous, inputStore.attachedFiles);
        inputStore.setAttachedFiles(readComposerAttachments(identity));
    }, [identity]);
}
