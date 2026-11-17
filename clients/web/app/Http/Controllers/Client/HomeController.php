<?php

namespace App\Http\Controllers\Client;

use App\Client\ChiefOfStaff;
use App\Client\ConversationOrder;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Leads;
use App\Http\Controllers\Controller;
use Illuminate\Http\RedirectResponse;

/**
 * Where the app opens. With the chief-of-staff slot on, on its conversation (ChiefOfStaff). With it off, on the
 * conversation that needs you, else the one that runs tasks, else the newest; with none yet, on a new conversation.
 */
class HomeController extends Controller
{
    /** Newest conversations looked through for one that needs you. */
    public const RECENT = 100;

    public function __invoke(DirectorClient $director, ChiefOfStaff $chief): RedirectResponse
    {
        $chiefId = $chief->conversationId();
        if ($chiefId !== null) {
            return redirect()->route('conversations.show', $chiefId);
        }

        try {
            // The recent conversations, to find one that needs you.
            $answer = $director->get('conversations', ['limit' => self::RECENT]);
        } catch (DirectorRefused|DirectorUnavailable) {
            // The conversation list says what is wrong with the Director and keeps trying.
            return redirect()->route('conversations.index');
        }
        $conversations = array_values(array_filter((array) ($answer['conversations'] ?? []), fn (mixed $conversation): bool => is_array($conversation) && is_string($conversation['conversation_id'] ?? null)));
        $leadIds = Leads::among($conversations);

        $first = ConversationOrder::sort($conversations, $leadIds)[0] ?? null;

        return $first === null
            ? redirect()->route('conversations.create')
            : redirect()->route('conversations.show', $first['conversation_id']);
    }
}
