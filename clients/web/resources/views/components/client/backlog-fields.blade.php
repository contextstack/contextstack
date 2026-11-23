@props(['blockers' => []])

{{-- The backlog item form's fields, bound to `form.*` on the page (App\Livewire\Forms\BacklogItemForm). --}}
<div class="space-y-4">
    <flux:input wire:model="form.title" :label="__('Title')" data-test="backlog-title" />
    <flux:textarea wire:model="form.body" :label="__('Details')" rows="4" data-test="backlog-body" />

    <div class="grid grid-cols-2 gap-4">
        <flux:select wire:model.live="form.status" :label="__('Status')" data-test="backlog-status-field">
            <flux:select.option value="next">{{ __('Next') }}</flux:select.option>
            <flux:select.option value="now">{{ __('Now') }}</flux:select.option>
            <flux:select.option value="blocked">{{ __('Blocked') }}</flux:select.option>
            <flux:select.option value="done">{{ __('Done') }}</flux:select.option>
            <flux:select.option value="dropped">{{ __('Dropped') }}</flux:select.option>
        </flux:select>
        <flux:input type="number" wire:model="form.priority" :label="__('Priority')" :description="__('Lower runs first.')" data-test="backlog-priority" />
    </div>

    <flux:fieldset>
        <flux:legend>{{ __('Blocked on') }}</flux:legend>
        <flux:text class="mb-2 text-sm">{{ __('Next picks an item only when every blocker has cleared. Owner and external blockers clear when you remove them; a job or item blocker clears when that job finishes or that item is done or dropped.') }}</flux:text>
        <div class="space-y-2">
            @foreach ($blockers as $index => $blocker)
                <div class="flex items-end gap-2" wire:key="blocker-{{ $index }}" data-test="backlog-blocker">
                    <flux:select wire:model="form.blockers.{{ $index }}.kind" class="w-36" :aria-label="__('Kind')">
                        <flux:select.option value="owner">{{ __('You') }}</flux:select.option>
                        <flux:select.option value="external">{{ __('External') }}</flux:select.option>
                        <flux:select.option value="item">{{ __('Another item') }}</flux:select.option>
                        <flux:select.option value="job">{{ __('A job') }}</flux:select.option>
                    </flux:select>
                    <flux:input wire:model="form.blockers.{{ $index }}.ref" class="flex-1" :aria-label="__('Reference')" :placeholder="__('owner, a name, an item id or a job id')" />
                    <flux:button size="sm" variant="ghost" icon="x-mark" wire:click="removeBlocker({{ $index }})" :aria-label="__('Remove blocker')" />
                </div>
            @endforeach
        </div>
        <flux:button size="sm" variant="ghost" icon="plus" class="mt-2" wire:click="addBlocker" data-test="add-blocker">{{ __('Add a blocker') }}</flux:button>
    </flux:fieldset>

    <flux:textarea wire:model="form.blockedReason" :label="__('Why it is blocked')" rows="2" />
    <flux:checkbox wire:model="form.stop" :label="__('Stop the heartbeat while this waits on me')" :description="__('With an owner blocker, the project\'s heartbeat pauses until you clear it.')" data-test="backlog-stop" />
</div>
