/**
 * AskAnswerCard — the omp ask tool's transcript answer card (spec 03 §5.4.1,
 * TUI parity ask.ts renderResult): one block per question with every offered
 * option marked selected/unselected, custom input and note lines, and the
 * "auto-selected after timeout" annotation that distinguishes automatic
 * answers from real user choices.
 */

import React from 'react';
import { cn } from '@/lib/utils';
import { Icon } from '@/components/icon/Icon';
import { useI18n } from '@/lib/i18n';
import type { AskAnswerEntry, AskAnswerModel } from './askToolDetails';

const AskEntryView: React.FC<{ entry: AskAnswerEntry }> = ({ entry }) => {
    const { t } = useI18n();
    // Prefer the full recorded option set; fall back to the selected labels
    // when the details omit the options array (TUI parity).
    const options = entry.options.length > 0 ? entry.options : entry.selectedOptions;
    const answered = entry.selectedOptions.length > 0
        || entry.customInput !== undefined
        || entry.note !== undefined;

    return (
        <div className="flex min-w-0 flex-col gap-1.5">
            <p className="whitespace-pre-wrap break-words text-sm leading-relaxed text-foreground">
                {entry.question}
            </p>
            {options.length > 0 ? (
                <div className="flex flex-wrap gap-1">
                    {options.map((label) => {
                        const selected = entry.selectedOptions.includes(label);
                        return (
                            <span
                                key={label}
                                className={cn(
                                    'inline-flex items-center gap-1 rounded-full border px-2 py-0.5 typography-meta',
                                    selected
                                        ? 'border-primary bg-primary/5 text-primary'
                                        : 'border-border/30 text-muted-foreground/70',
                                )}
                            >
                                {selected ? <Icon name="check" className="size-3 shrink-0" /> : null}
                                {label}
                            </span>
                        );
                    })}
                </div>
            ) : null}
            {entry.customInput !== undefined ? (
                <p className="whitespace-pre-wrap break-words typography-meta text-foreground">
                    <span className="text-muted-foreground">{t('chat.toolPart.askCustomAnswer')}: </span>
                    {entry.customInput}
                </p>
            ) : null}
            {entry.note !== undefined ? (
                <p className="whitespace-pre-wrap break-words typography-meta text-foreground">
                    <span className="text-muted-foreground">{t('chat.toolPart.askNote')}: </span>
                    {entry.note}
                </p>
            ) : null}
            {!answered ? (
                <p className="typography-meta text-muted-foreground">{t('chat.toolPart.askNoSelection')}</p>
            ) : null}
            {entry.timedOut ? (
                <p className="typography-micro text-muted-foreground">
                    {t('chat.toolPart.askAutoSelected')}
                </p>
            ) : null}
        </div>
    );
};

export const AskAnswerCard: React.FC<{ model: AskAnswerModel }> = ({ model }) => {
    const { t } = useI18n();

    if (model.kind === 'chatRedirect') {
        return (
            <div className="flex min-w-0 flex-col gap-1.5">
                <p className="typography-meta text-muted-foreground">{t('chat.toolPart.askChatRedirect')}</p>
                {model.questions.map((question, index) => (
                    <p
                        key={`${index}:${question}`}
                        className="whitespace-pre-wrap break-words typography-meta text-foreground"
                    >
                        {question}
                    </p>
                ))}
            </div>
        );
    }

    return (
        <div className="flex min-w-0 flex-col gap-3">
            {model.entries.length > 1 ? (
                <p className="typography-micro text-muted-foreground">
                    {t('chat.toolPart.askQuestionsCount', { count: model.entries.length })}
                </p>
            ) : null}
            {model.entries.map((entry) => (
                <AskEntryView key={entry.id} entry={entry} />
            ))}
        </div>
    );
};
