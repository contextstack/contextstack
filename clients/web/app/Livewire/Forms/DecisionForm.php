<?php

namespace App\Livewire\Forms;

use Livewire\Form;

/**
 * One project decision log entry as entered (CLIENT-API.md "Project decision log"): the decision on one line, why,
 * the alternatives considered with why each was not chosen, when to reopen it, and its sources. Reopen conditions
 * and sources are one per line here; the Director takes them as arrays.
 */
class DecisionForm extends Form
{
    public string $decision = '';

    public string $why = '';

    /** @var list<array{option: string, why_not: string}> */
    public array $alternatives = [];

    public string $reopenWhen = '';

    public string $sources = '';

    /**
     * @return array<string, mixed>
     */
    protected function rules(): array
    {
        return [
            'decision' => ['required', 'string', 'max:500', 'not_regex:/[\r\n]/'],
            'why' => ['required', 'string', 'max:20000'],
            'alternatives' => ['array', 'max:50'],
            'alternatives.*.option' => ['required', 'string', 'max:2000'],
            'alternatives.*.why_not' => ['required', 'string', 'max:20000'],
            'reopenWhen' => ['nullable', 'string', 'max:20000'],
            'sources' => ['nullable', 'string', 'max:20000'],
        ];
    }

    /**
     * @return array<string, string>
     */
    protected function messages(): array
    {
        return [
            'decision.not_regex' => __('The decision is one line.'),
            'alternatives.*.option.required' => __('Name the alternative.'),
            'alternatives.*.why_not.required' => __('Say why it was not chosen.'),
        ];
    }

    /**
     * Fill from an entry the Director holds.
     *
     * @param  array<string, mixed>  $entry
     */
    public function fillFrom(array $entry): void
    {
        $this->decision = (string) ($entry['decision'] ?? '');
        $this->why = (string) ($entry['why'] ?? '');
        $this->alternatives = array_values(array_map(fn (array $alternative): array => [
            'option' => (string) ($alternative['option'] ?? ''),
            'why_not' => (string) ($alternative['why_not'] ?? ''),
        ], array_filter((array) ($entry['alternatives'] ?? []), 'is_array')));
        $this->reopenWhen = implode("\n", array_filter((array) ($entry['reopen_when'] ?? []), 'is_string'));
        $this->sources = implode("\n", array_filter((array) ($entry['sources'] ?? []), 'is_string'));
        $this->resetValidation();
    }

    public function addAlternative(): void
    {
        $this->alternatives[] = ['option' => '', 'why_not' => ''];
    }

    public function removeAlternative(int $index): void
    {
        $this->alternatives = array_values(array_filter($this->alternatives, fn (int $key): bool => $key !== $index, ARRAY_FILTER_USE_KEY));
    }

    /**
     * The entry's content as the Director takes it, once valid.
     *
     * @return array{decision: string, why: string, alternatives: list<array{option: string, why_not: string}>, reopen_when: list<string>, sources: list<string>}
     */
    public function content(): array
    {
        $this->validate();

        return [
            'decision' => trim($this->decision),
            'why' => trim($this->why),
            'alternatives' => array_map(fn (array $alternative): array => [
                'option' => trim($alternative['option']),
                'why_not' => trim($alternative['why_not']),
            ], $this->alternatives),
            'reopen_when' => self::lines($this->reopenWhen),
            'sources' => self::lines($this->sources),
        ];
    }

    /**
     * Only the fields that differ from the entry held, for a revise.
     *
     * @param  array<string, mixed>  $entry
     * @return array<string, mixed>
     */
    public function changesFrom(array $entry): array
    {
        $changes = [];
        foreach ($this->content() as $field => $value) {
            if (($entry[$field] ?? ($field === 'decision' || $field === 'why' ? '' : [])) !== $value) {
                $changes[$field] = $value;
            }
        }

        return $changes;
    }

    /**
     * @return list<string>
     */
    private static function lines(string $text): array
    {
        return array_values(array_filter(array_map('trim', preg_split('/\R/', $text) ?: []), fn (string $line): bool => $line !== ''));
    }
}
