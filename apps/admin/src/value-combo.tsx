import React, { useEffect, useState } from 'react';
import { ComboBox } from '@carbon/react';
import { api, type UsageDimension, type UsageSliceQuery } from './api';
import { Button, FormField, Select } from './ui';

// A filter's value picked from the values the slice has, instead of an id
// pasted from another page: a Carbon combo box that narrows its list as you
// type, and still takes a typed value it does not list. Shared by the usage
// dashboard and the Traces page.

export type ValueOption = { value: string; label: string };

/** Most values one lookup lists (the series API's own limit). */
export const VALUE_LIMIT = 20;

/** The request statuses the usage API knows. */
export const statusValues: ValueOption[] = [
  { value: 'succeeded', label: 'Succeeded' },
  { value: 'failed', label: 'Failed' },
  { value: 'failed-over', label: 'Failed over' },
  { value: 'pending', label: 'Pending' },
  { value: 'expired', label: 'Expired before the call' },
  { value: 'unknown', label: 'Result unknown' },
];

/** How a request was finalized, as the gateway records it; the slice's own
 * values are offered as well. */
export const outcomeValues: ValueOption[] = ['settled', 'unknown', 'bound-breach', 'input-count-drift', 'expired', 'failed-over', 'refused', 'operator-resolved']
  .map(value => ({ value, label: value }));

/** Whether an option matches what was typed: by name or by value. */
export function matchesTyped(option: ValueOption, typed: string): boolean {
  const text = typed.trim().toLowerCase();
  return !text || option.label.toLowerCase().includes(text) || option.value.toLowerCase().includes(text);
}

/** The value a typed text stands for: the listed option it names, else
 * the text itself. */
export function typedValue(options: ValueOption[], typed: string): ValueOption | null {
  const text = typed.trim();
  if (!text) return null;
  return options.find(option => option.label === text || option.value === text) ?? { value: text, label: text };
}

/** The values of `dimension` in a slice, its most used first, once
 * `enabled`. `query` is the slice without that dimension's own filter;
 * null asks for nothing. */
export function useDimensionValues(organizationId: string, dimension: UsageDimension, query: UsageSliceQuery | null, enabled: boolean, labelOf: (value: string, label: string | null) => string) {
  const key = query && enabled ? JSON.stringify([organizationId, dimension, query]) : null;
  const [loaded, setLoaded] = useState<{ key: string; options: ValueOption[]; failed: boolean } | null>(null);
  useEffect(() => {
    if (!key || !query) return;
    let current = true;
    // Weeks: a lookup needs only the totals, and a year is 53 buckets.
    api.usageSeries(organizationId, { ...query, split_by: dimension, granularity: 'week', limit: VALUE_LIMIT })
      .then(report => report.series
        .filter(series => !series.other && series.value !== null)
        .map(series => ({ value: series.value!, label: labelOf(series.value!, series.label) })))
      .then(
        options => { if (current) setLoaded({ key, options, failed: false }); },
        () => { if (current) setLoaded({ key, options: [], failed: true }); },
      );
    return () => { current = false; };
  }, [key]); // eslint-disable-line react-hooks/exhaustive-deps
  const ready = loaded && loaded.key === key ? loaded : null;
  return { options: ready?.options ?? [], loading: Boolean(key) && !ready, failed: ready?.failed ?? false };
}

/** A typeable list of values: picks one it lists, or takes the typed text. */
export function ValueCombo({ id, label, options, placeholder, helperText, onText, onPick, onFocus }: {
  id: string;
  label: string;
  options: ValueOption[];
  placeholder?: string;
  helperText?: string;
  /** The text in the box, or the picked option's name. */
  onText: (text: string) => void;
  /** The option picked from the list, kept whole: names can repeat (two
   * service accounts may share one), values cannot. Null when cleared. */
  onPick?: (option: ValueOption | null) => void;
  onFocus?: () => void;
}) {
  return (
    <div className="value-combo" onFocus={onFocus}>
      <ComboBox<ValueOption>
        id={id}
        titleText={label}
        items={options}
        itemToString={item => item?.label ?? ''}
        shouldFilterItem={({ item, inputValue }) => matchesTyped(item, inputValue ?? '')}
        onChange={({ selectedItem, inputValue }) => {
          onPick?.(selectedItem ?? null);
          onText(selectedItem?.label ?? inputValue ?? '');
        }}
        onInputChange={text => onText(text)}
        allowCustomValue
        placeholder={placeholder}
        helperText={helperText}
        size="md"
      />
    </div>
  );
}

/** Adds a filter: a dimension, then one of its values. */
export function ValueFilterForm<D extends UsageDimension>({ organizationId, idPrefix, dimensions, initial, queryFor, labelOf, onAdd }: {
  organizationId: string;
  idPrefix: string;
  dimensions: [D, string][];
  initial: D;
  /** The slice the values are looked up in, without the dimension's own filter. */
  queryFor: (dimension: D) => UsageSliceQuery | null;
  labelOf: (dimension: D, value: string, label: string | null) => string;
  onAdd: (dimension: D, value: string, label: string) => void;
}) {
  const [dimension, setDimension] = useState<D>(initial);
  const [text, setText] = useState('');
  // The option picked from the list, while the box still shows its name.
  const [picked, setPicked] = useState<ValueOption | null>(null);
  const changeText = (next: string) => {
    setText(next);
    setPicked(current => (current && current.label === next ? current : null));
  };
  // The box starts over after each filter it adds.
  const [round, setRound] = useState(0);
  // Values are looked up once the box is first used, not on every page load.
  const [wanted, setWanted] = useState(false);
  const fixed = dimension === 'status' ? statusValues : null;
  const query = fixed ? null : queryFor(dimension);
  const found = useDimensionValues(organizationId, dimension, query, wanted, (value, label) => labelOf(dimension, value, label));
  const options = fixed ?? (dimension === 'outcome'
    ? [...found.options, ...outcomeValues.filter(known => !found.options.some(option => option.value === known.value))]
    : found.options);
  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    const chosen = picked && picked.label === text.trim() ? picked : typedValue(options, text);
    if (!chosen) return;
    onAdd(dimension, chosen.value, chosen.label);
    setText('');
    setPicked(null);
    setRound(value => value + 1);
  };
  const helperText = found.loading ? 'Loading values…' : found.failed ? 'Values could not be loaded: type one.' : undefined;
  return (
    <form className="report-controls add-filter" aria-label="Add a filter" onSubmit={submit}>
      <div className="audit-filter"><FormField label="Filter by" htmlFor={`${idPrefix}-dimension`}>
        <Select id={`${idPrefix}-dimension`} value={dimension} onChange={event => { setDimension(event.target.value as D); setText(''); setPicked(null); setRound(value => value + 1); }}>
          {dimensions.map(([key, label]) => <option key={key} value={key}>{label}</option>)}
        </Select>
      </FormField></div>
      <div className="audit-filter value-filter">
        <ValueCombo
          key={`${dimension}:${round}`}
          id={`${idPrefix}-value`}
          label="Value"
          options={options}
          placeholder={fixed ? 'Type to search' : 'Type to search, or paste a value'}
          helperText={helperText}
          onText={changeText}
          onPick={setPicked}
          onFocus={() => setWanted(true)}
        />
      </div>
      <Button variant="ghost" type="submit">Add filter</Button>
    </form>
  );
}
