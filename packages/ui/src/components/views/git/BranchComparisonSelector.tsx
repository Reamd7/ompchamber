import { useState } from 'react';
import { Icon } from '@/components/icon/Icon';
import { Button } from '@/components/ui/button';
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { DropdownMenu, DropdownMenuContent, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { dropdownTriggerVariants } from '@/components/ui/dropdown-trigger';
import { useI18n } from '@/lib/i18n';
import { rankByQuery } from '@/lib/search/fuzzySearch';
import { cn } from '@/lib/utils';
import { branchRefLabel } from './baseBranch';

interface BranchComparisonSelectorProps {
  branches: readonly string[];
  currentBranch: string | null;
  base: string | null;
  onSelect: (ref: string) => void;
}

export function BranchComparisonSelector({ branches, currentBranch, base, onSelect }: BranchComparisonSelectorProps) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState('');
  const label = base ? branchRefLabel(base) : t('gitView.pr.field.baseBranch');

  return (
    <DropdownMenu open={open} onOpenChange={(nextOpen) => {
      setOpen(nextOpen);
      if (!nextOpen) setSearch('');
    }}>
      <DropdownMenuTrigger asChild>
        <Button
          variant="outline"
          className={cn(dropdownTriggerVariants({ size: 'sm' }), 'min-w-0 max-w-48')}
          aria-label={t('gitView.pr.field.baseBranch')}
          title={label}
          disabled={!currentBranch}
        >
          <Icon name="git-branch" className="size-3.5" />
          <span className="truncate">{label}</span>
          <Icon name="arrow-down-s" className="size-3.5" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-72 max-w-[calc(100vw-2rem)] p-0">
        <Command shouldFilter={false} onKeyDown={(event) => {
          if (event.key !== 'Escape') event.stopPropagation();
        }}>
          <CommandInput
            autoFocus
            value={search}
            onValueChange={setSearch}
            placeholder={t('gitView.branch.searchPlaceholder')}
            aria-label={t('gitView.branch.searchPlaceholder')}
          />
          <CommandList>
            <CommandEmpty>{t('gitView.branch.empty')}</CommandEmpty>
            <CommandGroup>
              {open && rankByQuery(
                [...new Set(branches)]
                  .filter((name) => name !== currentBranch)
                  .sort()
                  .map((name) => ({
                    ref: name.startsWith('remotes/') ? `refs/${name}` : `refs/heads/${name}`,
                    label: branchRefLabel(name),
                  })),
                search,
                (branch) => [branch.label],
              ).map((branch) => (
                <CommandItem key={branch.ref} value={branch.ref} onSelect={() => {
                  onSelect(branch.ref);
                  setOpen(false);
                  setSearch('');
                }}>
                  <span className="min-w-0 flex-1 truncate" title={branch.ref}>{branch.label}</span>
                  {(branch.ref === base || branch.label === base) && <Icon name="check" className="size-3.5" />}
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
