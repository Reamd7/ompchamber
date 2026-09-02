import { describe, expect, test } from 'bun:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

import { I18nProvider } from '@/lib/i18n';
import { StrippedToolCallsLine } from './StrippedToolCallsLine';

const render = (element: React.ReactElement): string =>
    renderToStaticMarkup(<I18nProvider>{element}</I18nProvider>);

describe('StrippedToolCallsLine', () => {
    test('renders the elided-activity line for a positive count', () => {
        const markup = render(<StrippedToolCallsLine count={3} />);
        expect(markup).toContain('data-omp-stripped-tool-calls="3"');
        expect(markup).toContain('3 tool calls elided');
    });

    test('renders nothing for zero or non-finite counts', () => {
        expect(render(<StrippedToolCallsLine count={0} />)).toBe('');
        expect(render(<StrippedToolCallsLine count={Number.NaN} />)).toBe('');
    });
});
