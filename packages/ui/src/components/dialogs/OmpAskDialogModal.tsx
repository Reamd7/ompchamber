/**
 * OmpAskDialogModal — the omp ask-dialog body (spec 03 §5.4.1, TUI parity:
 * one card per question, radio/checkbox shapes, "(Recommended)" badge,
 * Other/Chat-about-this persistent actions, per-question Next on multi-
 * question dialogs).
 *
 * Submit contract mirrors the tool semantics: multi-select may submit empty
 * ("select none"); single-select requires a selection or custom input
 * (an empty single answer is a cancel on the tool side, so the UI gates it).
 * "Chat about this" responds `{kind:"chat"}`; the dialog closes.
 */

import React from 'react';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Radio } from '@/components/ui/radio';
import { Icon } from '@/components/icon/Icon';
import { useI18n } from '@/lib/i18n';
import { toast } from '@/components/ui';
import { cn } from '@/lib/utils';
import type { OmpPendingDialog, OmpAskQuestion } from '@/lib/api/omp';
import { ompDialogController } from '@/sync/omp-dialog-controller';
import { useOmpDialogUi } from '@/sync/useOmpDialogStore';

interface OmpAskDialogModalProps {
  directory: string;
  dialog: Extract<OmpPendingDialog, { kind: 'ask' }>;
}

type TabKey = string;
const SUMMARY_TAB = 'summary';

interface AnswerDraft {
  selectedOptions: string[];
  customMode: boolean;
  customText: string;
  note: string;
}

const emptyDraft = (): AnswerDraft => ({ selectedOptions: [], customMode: false, customText: '', note: '' });

const isAnswered = (question: OmpAskQuestion, draft: AnswerDraft): boolean => {
  if (draft.customMode) return draft.customText.trim().length > 0;
  if (question.multi === true) return true; // "select none" is legal
  return draft.selectedOptions.length > 0;
};

export const OmpAskDialogModal: React.FC<OmpAskDialogModalProps> = ({ directory, dialog }) => {
  const { t } = useI18n();
  const ui = useOmpDialogUi(directory, dialog.id);
  const questions = dialog.ask.questions;
  const [activeTab, setActiveTab] = React.useState<TabKey>('0');
  const [drafts, setDrafts] = React.useState<Record<number, AnswerDraft>>(() => ({}));
  const inflight = ui?.respondInflight === true;

  React.useEffect(() => {
    setActiveTab('0');
    setDrafts({});
  }, [dialog.id]);

  const isSummaryTab = activeTab === SUMMARY_TAB;
  const activeIndex = isSummaryTab ? -1 : Math.max(0, Math.min(questions.length - 1, Number(activeTab) || 0));
  const activeQuestion = isSummaryTab ? null : questions[activeIndex];
  const draftOf = (index: number): AnswerDraft => drafts[index] ?? emptyDraft();

  const unanswered = React.useMemo(() => {
    const pending: number[] = [];
    questions.forEach((question, index) => {
      if (!isAnswered(question, draftOf(index))) pending.push(index);
    });
    return pending;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [questions, drafts]);

  const allAnswered = unanswered.length === 0 && questions.length > 0;

  const respond = React.useCallback(async (
    result: Parameters<typeof ompDialogController.respond>[2],
  ) => {
    await ompDialogController.respond(directory, dialog.id, result);
  }, [dialog.id, directory]);

  const toggleOption = (label: string): void => {
    if (activeQuestion === null) return;
    setDrafts((prev) => {
      const draft = draftOf(activeIndex);
      const next: AnswerDraft = { ...draft, customMode: false };
      if (activeQuestion.multi === true) {
        next.selectedOptions = draft.selectedOptions.includes(label)
          ? draft.selectedOptions.filter((item) => item !== label)
          : [...draft.selectedOptions, label];
      } else {
        next.selectedOptions = [label];
      }
      return { ...prev, [activeIndex]: next };
    });
  };

  const chooseCustom = (): void => {
    setDrafts((prev) => ({
      ...prev,
      [activeIndex]: { ...draftOf(activeIndex), customMode: true, selectedOptions: [] },
    }));
  };

  const patchDraft = (patch: Partial<AnswerDraft>): void => {
    setDrafts((prev) => ({ ...prev, [activeIndex]: { ...draftOf(activeIndex), ...patch } }));
  };

  const submit = (): void => {
    if (!allAnswered) return;
    const results = questions.map((question, index) => {
      const draft = draftOf(index);
      return {
        id: question.id,
        selectedOptions: draft.customMode ? [] : draft.selectedOptions,
        ...(draft.customMode && draft.customText.trim().length > 0 ? { customInput: draft.customText.trim() } : {}),
        ...(draft.note.trim().length > 0 ? { note: draft.note.trim() } : {}),
      };
    });
    void respond({ kind: 'ask', results });
  };

  const chatAboutThis = (): void => {
    void respond({ kind: 'chat' });
  };

  const tabs = React.useMemo(() => {
    const questionTabs = questions.map((question, index) => ({
      value: String(index),
      label: question.header?.trim() || `Q${index + 1}`,
    }));
    if (questions.length > 1) {
      questionTabs.push({ value: SUMMARY_TAB, label: t('dialogs.omp.ask.summaryTab') });
    }
    return questionTabs;
  }, [questions, t]);

  const answerDisplay = (index: number): string => {
    const draft = draftOf(index);
    if (draft.customMode) return draft.customText.trim() || t('dialogs.omp.ask.noAnswer');
    return draft.selectedOptions.length > 0 ? draft.selectedOptions.join(', ') : t('dialogs.omp.ask.noAnswer');
  };

  return (
    <div className="flex flex-col gap-4">
      {tabs.length > 1 ? (
        <div className="flex flex-wrap gap-1" role="tablist">
          {tabs.map((tab) => (
            <button
              key={tab.value}
              type="button"
              role="tab"
              aria-selected={activeTab === tab.value}
              className={cn(
                'rounded-full border px-2.5 py-1 typography-meta transition-colors',
                activeTab === tab.value
                  ? 'border-primary text-primary'
                  : 'border-border/40 text-muted-foreground hover:text-foreground',
              )}
              onClick={() => setActiveTab(tab.value)}
            >
              {tab.value !== SUMMARY_TAB && unanswered.includes(Number(tab.value)) ? '• ' : ''}
              {tab.label}
            </button>
          ))}
        </div>
      ) : null}

      {activeQuestion ? (
        <div className="flex flex-col gap-3">
          {activeQuestion.header ? (
            <h4 className="typography-meta font-medium text-muted-foreground">{activeQuestion.header}</h4>
          ) : null}
          <p className="whitespace-pre-wrap text-sm leading-relaxed text-foreground">{activeQuestion.question}</p>

          <div className="flex flex-col gap-1.5" role={activeQuestion.multi === true ? 'group' : 'radiogroup'}>
            {activeQuestion.options.map((option) => {
              const checked = !draftOf(activeIndex).customMode && draftOf(activeIndex).selectedOptions.includes(option.label);
              return (
                <button
                  key={option.label}
                  type="button"
                  role={activeQuestion.multi === true ? 'checkbox' : 'radio'}
                  aria-checked={checked}
                  className={cn(
                    'flex w-full items-start gap-2 rounded border p-2 text-left transition-colors',
                    checked ? 'border-primary bg-primary/5' : 'border-border/30 hover:border-border',
                    draftOf(activeIndex).customMode && 'opacity-50',
                  )}
                  disabled={draftOf(activeIndex).customMode}
                  onClick={() => toggleOption(option.label)}
                >
                  {activeQuestion.multi === true ? (
                    <Checkbox checked={checked} onChange={() => toggleOption(option.label)} className="mt-0.5" />
                  ) : (
                    <Radio checked={checked} onChange={() => toggleOption(option.label)} className="mt-0.5" />
                  )}
                  <span className="flex min-w-0 flex-col gap-0.5">
                    <span className="flex flex-wrap items-center gap-2">
                      <span className="text-sm text-foreground">{option.label}</span>
                      {activeQuestion.recommended === option.label ? (
                        <span className="rounded border border-border/40 px-1 py-px typography-meta text-muted-foreground">
                          {t('dialogs.omp.ask.recommended')}
                        </span>
                      ) : null}
                    </span>
                    {option.description ? (
                      <span className="typography-meta text-muted-foreground">{option.description}</span>
                    ) : null}
                    {option.preview ? (
                      <span className="overflow-x-auto rounded border border-border/30 bg-muted/20 p-1.5 font-mono text-[11px] text-muted-foreground">
                        {option.preview}
                      </span>
                    ) : null}
                  </span>
                </button>
              );
            })}
          </div>

          {draftOf(activeIndex).customMode ? (
            <div className="flex flex-col gap-1.5">
              <textarea
                value={draftOf(activeIndex).customText}
                onChange={(event) => patchDraft({ customText: event.target.value })}
                rows={2}
                autoFocus
                placeholder={t('dialogs.omp.ask.otherPlaceholder')}
                className="w-full resize-none rounded border border-border/30 bg-transparent px-2 py-1 typography-meta text-foreground outline-none transition-colors focus:border-primary"
              />
              <input
                value={draftOf(activeIndex).note}
                onChange={(event) => patchDraft({ note: event.target.value })}
                placeholder={t('dialogs.omp.ask.note')}
                className="w-full rounded border border-border/30 bg-transparent px-2 py-1 typography-meta text-foreground outline-none transition-colors focus:border-primary"
              />
            </div>
          ) : (
            <button
              type="button"
              className="self-start text-sm text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
              onClick={chooseCustom}
            >
              {t('dialogs.omp.ask.other')}
            </button>
          )}
        </div>
      ) : (
        <ul className="flex flex-col gap-2">
          {questions.map((question, index) => (
            <li key={question.id} className="rounded border border-border/30 p-2">
              <p className="typography-meta text-muted-foreground">{question.header || `Q${index + 1}`}</p>
              <p className="truncate text-sm text-foreground">{question.question}</p>
              <p className="typography-meta text-muted-foreground">{answerDisplay(index)}</p>
            </li>
          ))}
        </ul>
      )}

      {ui?.respondError ? (
        <p className="text-status-error typography-meta" role="alert">
          {t('dialogs.omp.ask.respondFailed')}: {ui.respondError}
        </p>
      ) : null}

      <div className="flex items-center justify-between gap-2">
        <Button variant="ghost" size="sm" disabled={inflight} onClick={chatAboutThis}>
          {t('dialogs.omp.ask.chatAboutThis')}
        </Button>
        <div className="flex items-center gap-2">
          {!isSummaryTab && questions.length > 1 && !allAnswered ? (
            <Button
              variant="outline"
              size="sm"
              disabled={inflight || !isAnswered(activeQuestion ?? questions[activeIndex], draftOf(activeIndex))}
              onClick={() => setActiveTab(String((activeIndex + 1) % questions.length))}
            >
              {t('dialogs.omp.ask.next')}
            </Button>
          ) : null}
          <Button
            size="sm"
            disabled={inflight || !allAnswered}
            onClick={submit}
          >
            {inflight ? <Icon name="loader-4" className="size-4 animate-spin" /> : null}
            {t('dialogs.omp.ask.submit')}
          </Button>
        </div>
      </div>
    </div>
  );
};
