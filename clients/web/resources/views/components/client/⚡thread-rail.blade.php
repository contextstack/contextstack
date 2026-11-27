<?php

use App\Client\ChiefOfStaff;
use App\Client\ConversationOrder;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Client\Leads;
use Livewire\Attributes\Lazy;
use Livewire\Attributes\Locked;
use Livewire\Attributes\On;
use Livewire\Component;

/*
 * The sidebar's conversation list, laid out like Codex's: New chat and the chief of staff at the top, then collapsible
 * groups: one per project (its conversations, a few at a time) and Recents (those in no project). Conversations that
 * run tasks carry a wrench; a Bots group comes once the Director has bot records (a persistent identity with its own
 * memory), which a conversation that runs tasks is not. Within a group,
 * those waiting on you come first, then those that run tasks, then the newest. It refreshes on a timer rather than
 * holding its own event stream, since each open stream holds a PHP worker. Which groups are collapsed is this
 * device's own look (localStorage); what is in them all comes from the Director.
 */
new #[Lazy] class extends Component
{
    /** Most conversations listed under Recents. */
    public const SHOWN = 30;

    /** A project's conversations shown before its "Show more". */
    public const PROJECT_SHOWN = 5;

    /** Newest conversations fetched to fill the groups (those needing you come first, whatever their age). */
    public const FETCHED = 100;

    /** @var array{id: string, name: string, needs_you: bool, running: bool}|null */
    #[Locked]
    public ?array $chief = null;

    /** @var list<array{id: string, name: string, threads: list<array{id: string, name: string, lead: bool, needs_you: bool, running: bool}>}> */
    #[Locked]
    public array $projects = [];

    /** @var list<array{id: string, name: string, lead: bool, needs_you: bool, running: bool}> */
    #[Locked]
    public array $threads = [];

    public function mount(): void
    {
        $this->refresh();
    }

    /** Also when a page archives, unarchives or deletes a conversation, so the list does not wait for its next poll. */
    #[On('threads-changed')]
    public function refresh(): void
    {
        try {
            $director = app(DirectorClient::class);
            $recent = (array) ($director->get('conversations', ['limit' => self::FETCHED])['conversations'] ?? []);
            // A long-lived lead stays listed however many newer conversations there are.
            $leads = (array) ($director->get('conversations', ['lead' => '1', 'limit' => Leads::FETCHED])['conversations'] ?? []);
        } catch (DirectorRefused|DirectorUnavailable|NotConnected) {
            return;
        }
        // Project names only label the groups: without them a group shows its id.
        $projectNames = [];
        try {
            foreach ((array) ($director->get('projects')['projects'] ?? []) as $project) {
                if (is_array($project) && is_string($project['project_id'] ?? null)) {
                    $projectNames[$project['project_id']] = is_string($project['name'] ?? null) && $project['name'] !== '' ? $project['name'] : $project['project_id'];
                }
            }
        } catch (DirectorRefused|DirectorUnavailable) {
        }

        $conversations = [];
        foreach ([...$recent, ...$leads] as $conversation) {
            if (is_array($conversation) && is_string($conversation['conversation_id'] ?? null)) {
                $conversations[$conversation['conversation_id']] = $conversation;
            }
        }

        // The chief of staff sits above the groups, not in them.
        $chiefId = app(ChiefOfStaff::class)->conversationId();
        $chiefRecord = $chiefId !== null ? ($conversations[$chiefId] ?? null) : null;
        $this->chief = $chiefId === null ? null : [
            'id' => $chiefId,
            'name' => app(ChiefOfStaff::class)->slot()['name'],
            'needs_you' => is_array($chiefRecord) && ConversationOrder::needsYou($chiefRecord),
            'running' => is_array($chiefRecord) && self::running($chiefRecord),
        ];
        unset($conversations[(string) $chiefId]);

        $conversations = array_values($conversations);
        $leadIds = Leads::among($conversations);
        $byProject = [];
        $loose = [];
        foreach (ConversationOrder::sort($conversations, $leadIds) as $conversation) {
            $thread = [
                'id' => $conversation['conversation_id'],
                'name' => ConversationOrder::name($conversation),
                'lead' => in_array($conversation['conversation_id'], $leadIds, true),
                'needs_you' => ConversationOrder::needsYou($conversation),
                'running' => self::running($conversation),
            ];
            $projectId = $conversation['project_id'] ?? null;
            if (is_string($projectId) && $projectId !== '') {
                $byProject[$projectId][] = $thread;
            } else {
                $loose[] = $thread;
            }
        }

        // Projects in the order of their first listed conversation (needs you, runs tasks, newest).
        $this->projects = [];
        foreach ($byProject as $projectId => $threads) {
            $this->projects[] = ['id' => $projectId, 'name' => $projectNames[$projectId] ?? $projectId, 'threads' => array_slice($threads, 0, 20)];
        }
        $this->threads = array_slice($loose, 0, self::SHOWN);
    }

    /**
     * @param  array<string, mixed>  $conversation
     */
    private static function running(array $conversation): bool
    {
        return in_array($conversation['state'] ?? null, ['running', 'summarising'], true);
    }

    public function placeholder(): string
    {
        return '<div class="px-2 py-1 text-xs text-zinc-500">'.e(__('Loading threads…')).'</div>';
    }
};
?>

<div class="grid grid-cols-1 gap-px" wire:poll.15s.visible="refresh" data-test="thread-rail">
    @capable('start_work')
    <a href="{{ route('conversations.create') }}" wire:navigate wire:current.exact="cs-thread-current"
       class="flex h-8 items-center gap-2 rounded-lg px-2 text-sm text-zinc-700 hover:bg-black/5 dark:text-zinc-300 dark:hover:bg-white/[.06] [&.cs-thread-current]:bg-black/[.07] dark:[&.cs-thread-current]:bg-white/10"
       data-test="rail-new-chat">
        <flux:icon.pencil-square variant="micro" class="size-4 shrink-0" />
        <span>{{ __('New chat') }}</span>
    </a>
    @endcapable
    @if ($chief !== null)
        <a href="{{ route('conversations.show', $chief['id']) }}" wire:navigate.hover wire:current="cs-thread-current"
           class="flex h-8 items-center gap-2 rounded-lg px-2 text-sm font-medium text-zinc-800 hover:bg-black/5 dark:text-zinc-200 dark:hover:bg-white/[.06] [&.cs-thread-current]:bg-black/[.07] dark:[&.cs-thread-current]:bg-white/10"
           data-test="rail-chief-of-staff">
            <flux:icon.sparkles variant="micro" class="size-4 shrink-0 text-amber-500" />
            <span class="min-w-0 flex-1 truncate">{{ $chief['name'] }}</span>
            @if ($chief['needs_you'])
                <span class="size-1.5 shrink-0 rounded-full bg-amber-500" title="{{ __('Needs you') }}" data-test="rail-needs-you"></span>
            @elseif ($chief['running'])
                <span class="size-1.5 shrink-0 animate-pulse rounded-full bg-blue-500" title="{{ __('Working') }}" data-test="rail-running"></span>
            @endif
        </a>
    @endif

    {{-- Collapsible groups; whether each is open is this device's look, kept in localStorage. --}}
    @if ($projects !== [])
        <div x-data="{ open: (() => { try { return localStorage.getItem('cs.rail.projects') !== 'closed' } catch (e) { return true } })() }" class="mt-3" data-test="rail-projects">
            <button type="button" x-on:click="open = ! open; try { localStorage.setItem('cs.rail.projects', open ? 'open' : 'closed') } catch (e) {}"
                    x-bind:aria-expanded="open" class="flex w-full items-center gap-1 px-2 pb-1 text-xs font-medium text-zinc-500 hover:text-zinc-800 dark:hover:text-zinc-200">
                {{ __('Projects') }}
                <flux:icon.chevron-down variant="micro" class="size-3.5 transition-transform" x-bind:class="open || '-rotate-90'" />
            </button>
            <div x-show="open" class="grid grid-cols-1 gap-px">
                @foreach ($projects as $project)
                    <div x-data="{ open: (() => { try { return localStorage.getItem({{ Js::from('cs.rail.project.'.$project['id']) }}) !== 'closed' } catch (e) { return true } })(), more: false }"
                         wire:key="rail-project-{{ $project['id'] }}" data-test="rail-project">
                        <button type="button" x-on:click="open = ! open; try { localStorage.setItem({{ Js::from('cs.rail.project.'.$project['id']) }}, open ? 'open' : 'closed') } catch (e) {}"
                                x-bind:aria-expanded="open"
                                class="flex h-8 w-full items-center gap-2 rounded-lg px-2 text-sm text-zinc-700 hover:bg-black/5 dark:text-zinc-300 dark:hover:bg-white/[.06]">
                            <flux:icon.folder variant="micro" class="size-4 shrink-0 text-zinc-400" x-show="! open" />
                            <flux:icon.folder-open variant="micro" class="size-4 shrink-0 text-zinc-400" x-show="open" />
                            <span class="min-w-0 flex-1 truncate text-start">{{ $project['name'] }}</span>
                        </button>
                        <div x-show="open" class="grid grid-cols-1 gap-px">
                            @foreach ($project['threads'] as $index => $thread)
                                <x-client.thread-link :thread="$thread" indent x-show="{{ $index < $this::PROJECT_SHOWN ? 'true' : 'more' }}" />
                            @endforeach
                            @if (count($project['threads']) > $this::PROJECT_SHOWN)
                                <button type="button" x-show="! more" x-on:click="more = true" class="h-7 ps-7 text-start text-xs text-zinc-500 hover:text-zinc-800 dark:hover:text-zinc-200" data-test="rail-show-more">{{ __('Show more') }}</button>
                            @endif
                        </div>
                    </div>
                @endforeach
            </div>
        </div>
    @endif

    <div x-data="{ open: (() => { try { return localStorage.getItem('cs.rail.recents') !== 'closed' } catch (e) { return true } })() }" class="mt-3" data-test="rail-recents">
        <button type="button" x-on:click="open = ! open; try { localStorage.setItem('cs.rail.recents', open ? 'open' : 'closed') } catch (e) {}"
                x-bind:aria-expanded="open" class="flex w-full items-center gap-1 px-2 pb-1 text-xs font-medium text-zinc-500 hover:text-zinc-800 dark:hover:text-zinc-200">
            {{ __('Recents') }}
            <flux:icon.chevron-down variant="micro" class="size-3.5 transition-transform" x-bind:class="open || '-rotate-90'" />
        </button>
        <div x-show="open" class="grid grid-cols-1 gap-px">
            @forelse ($threads as $thread)
                <x-client.thread-link :thread="$thread" />
            @empty
                <div class="px-2 text-xs text-zinc-500">{{ __('No conversations yet.') }}</div>
            @endforelse
        </div>
    </div>
</div>
