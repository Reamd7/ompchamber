import { describe, expect, test } from "bun:test"

import { buildRestoredAttachment } from "@/sync/input-store"
import { createChatDraftIdentity } from "./chatDraftPersistence"
import { clearComposerAttachments, readComposerAttachments, stashComposerAttachments } from "./composerAttachmentStash"

const identityA = createChatDraftIdentity("runtime", "/project", "ses_a")
const identityB = createChatDraftIdentity("runtime", "/project", "ses_b")
const draftIdentity = createChatDraftIdentity("runtime", "/project", null)

const makeFile = (filename: string) =>
  buildRestoredAttachment({ url: "data:image/png;base64,AAAA", mimeType: "image/png", filename })

describe("composerAttachmentStash", () => {
  test("attachments stashed under one identity do not leak into another", () => {
    const file = makeFile("shot.png")
    stashComposerAttachments(identityA, [file])

    expect(readComposerAttachments(identityB)).toEqual([])
    expect(readComposerAttachments(identityA)).toEqual([file])
  })

  test("a draft identity (null session) has its own stash slot", () => {
    const file = makeFile("draft.png")
    stashComposerAttachments(draftIdentity, [file])

    expect(readComposerAttachments(identityB)).toEqual([])
    expect(readComposerAttachments(draftIdentity)).toEqual([file])
  })

  test("stashing an empty list clears the identity's entry", () => {
    stashComposerAttachments(identityA, [makeFile("shot.png")])
    stashComposerAttachments(identityA, [])

    expect(readComposerAttachments(identityA)).toEqual([])
  })

  test("clearComposerAttachments drops the identity's entry", () => {
    stashComposerAttachments(identityB, [makeFile("shot.png")])
    clearComposerAttachments(identityB)

    expect(readComposerAttachments(identityB)).toEqual([])
  })

  test("null identity is a no-op for every operation", () => {
    stashComposerAttachments(null, [makeFile("shot.png")])
    clearComposerAttachments(null)

    expect(readComposerAttachments(null)).toEqual([])
    expect(readComposerAttachments(identityA)).toEqual([])
  })

  test("readComposerAttachments returns a copy so callers cannot mutate the stash", () => {
    const file = makeFile("shot.png")
    stashComposerAttachments(identityA, [file])

    const first = readComposerAttachments(identityA)
    first.pop()

    expect(readComposerAttachments(identityA)).toEqual([file])
  })
})
