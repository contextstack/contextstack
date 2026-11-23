@props(['alternatives' => []])

{{--
    The fields of a decision log entry, bound to the host page's DecisionForm `form`; the page supplies addAlternative()
    and removeAlternative(). Alternatives are the part most entries are missing, so each gets its own pair of fields.
--}}
<div class="space-y-5">
    <flux:input wire:model="form.decision" :label="__('Decision')" :description="__('One line.')" required data-test="field-decision" />
    <flux:textarea wire:model="form.why" :label="__('Why')" rows="3" required data-test="field-why" />

    <flux:fieldset>
        <flux:legend>{{ __('Alternatives considered') }}</flux:legend>
        <div class="space-y-3">
            @foreach ($alternatives as $index => $alternative)
                <div class="grid gap-2 rounded-lg border border-black/[.08] p-3 dark:border-white/[.08]" wire:key="alternative-{{ $index }}" data-test="field-alternative">
                    <div class="flex items-start gap-2">
                        <flux:input wire:model="form.alternatives.{{ $index }}.option" :placeholder="__('The option')" :aria-label="__('Alternative')" class="flex-1" />
                        <flux:button size="sm" variant="ghost" icon="x-mark" square wire:click="removeAlternative({{ $index }})" :aria-label="__('Remove this alternative')" />
                    </div>
                    <flux:textarea wire:model="form.alternatives.{{ $index }}.why_not" :placeholder="__('Why it was not chosen')" :aria-label="__('Why not')" rows="2" />
                    <flux:error name="form.alternatives.{{ $index }}.option" />
                    <flux:error name="form.alternatives.{{ $index }}.why_not" />
                </div>
            @endforeach
            <flux:button size="sm" variant="ghost" icon="plus" wire:click="addAlternative" data-test="add-alternative">{{ __('Add an alternative') }}</flux:button>
        </div>
    </flux:fieldset>

    <flux:textarea wire:model="form.reopenWhen" :label="__('Reopen when')" :description="__('One condition per line.')" rows="2" data-test="field-reopen" />
    <flux:textarea wire:model="form.sources" :label="__('Sources')" :description="__('One per line: an ADR, a date, a link.')" rows="2" data-test="field-sources" />
</div>
