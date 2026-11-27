<?php

namespace App\Native;

use App\Client\Json;
use Carbon\CarbonImmutable;
use Throwable;

/**
 * Words for browser action approvals (CLIENT-API.md "Action approvals", docs/design/browser-action-approvals.md): an
 * agent's browser asks to submit, post or buy on an HTTPS origin. The owner sees the exact target, every captured
 * field in order (hidden fields labelled, masked values as their length) and the screenshot taken at proposal time,
 * and decides before `expires_at`. Everything from the page is shown as plain text, with invisible or text-reordering
 * characters escaped.
 */
final class BrowserActions
{
    /**
     * @param  array<string, mixed>  $approval
     */
    public static function is(array $approval): bool
    {
        return ($approval['kind'] ?? null) === 'browser_action';
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    public static function headline(array $approval): string
    {
        return self::actionWords($approval).' on '.self::origin($approval);
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    public static function actionWords(array $approval): string
    {
        $action = Json::string($approval['action'] ?? null);

        return match ($action) {
            'submit' => 'Submit a form',
            'post' => 'Post',
            'buy' => 'Buy',
            default => 'Unknown action "'.ActionApprovals::visible($action).'"',
        };
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    public static function origin(array $approval): string
    {
        return ActionApprovals::visible(Json::string($approval['origin'] ?? null, '(no origin)'));
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    public static function target(array $approval): string
    {
        return ActionApprovals::visible(Json::string($approval['target'] ?? null, '(no target)'));
    }

    /**
     * Every captured field in the order the page had them, repeated names included: `name`, the label words, and
     * what was captured (the value, or how many characters were masked).
     *
     * @param  array<string, mixed>  $approval
     * @return list<array{name: string, labels: string, shown: string, masked: bool}>
     */
    public static function fields(array $approval): array
    {
        $fields = [];
        foreach (is_array($approval['values_summary'] ?? null) ? $approval['values_summary'] : [] as $entry) {
            $entry = Json::record($entry);
            $labels = [];
            if (($entry['hidden'] ?? null) === true) {
                $labels[] = 'hidden field';
            }
            if (($entry['excluded'] ?? null) === true) {
                $labels[] = 'excluded (per-load token)';
            }
            $value = $entry['value'] ?? null;
            $length = $entry['length'] ?? null;
            $fields[] = [
                'name' => ActionApprovals::visible(Json::string($entry['name'] ?? null, '(no name)')),
                'labels' => implode(' · ', $labels),
                'shown' => match (true) {
                    is_string($value) => $value === '' ? '(empty)' : ActionApprovals::visible($value),
                    is_int($length) => 'masked, '.$length.' '.($length === 1 ? 'character' : 'characters'),
                    default => '(not captured)',
                },
                'masked' => ! is_string($value),
            ];
        }

        return $fields;
    }

    /**
     * The Director's warning that fields were masked or hidden, or '' when it gives none.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function warning(array $approval): string
    {
        if (($approval['has_field_warning'] ?? null) !== true) {
            return '';
        }
        $count = $approval['masked_or_hidden_count'] ?? null;
        if (! is_int($count)) {
            return 'Some fields are masked or hidden — review the screenshot';
        }

        return $count.' '.($count === 1 ? 'field' : 'fields').' masked or hidden — review the screenshot';
    }

    /**
     * The screenshot's SHA-256, or '' when the record carries none in the expected form (64 lowercase hex).
     *
     * @param  array<string, mixed>  $approval
     */
    public static function sha(array $approval): string
    {
        $sha = $approval['screenshot_sha'] ?? null;

        return is_string($sha) && self::validSha($sha) ? $sha : '';
    }

    public static function validSha(mixed $sha): bool
    {
        return is_string($sha) && preg_match('/^[0-9a-f]{64}$/D', $sha) === 1;
    }

    /**
     * Whether the decision window has ended by this device's clock. A missing or unreadable expiry counts as open:
     * the Director decides.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function ended(array $approval): bool
    {
        $at = self::expiresAt($approval);

        return $at !== null && ! $at->isFuture();
    }

    /**
     * When the window ends, in words.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function window(array $approval): string
    {
        $at = self::expiresAt($approval);
        if ($at === null) {
            return 'The Director did not say when the window ends.';
        }
        $exact = $at->format('Y-m-d H:i').' UTC';
        if ($at->isFuture()) {
            return 'Decide by '.$exact.' ('.$at->diffForHumans().').';
        }

        return 'The window ended '.$at->diffForHumans().' ('.$exact.').';
    }

    /**
     * Where it stands, in words.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function status(array $approval): string
    {
        return match ($approval['status'] ?? null) {
            'needs_owner' => self::ended($approval) ? 'The window has ended' : 'Needs you',
            'approved' => 'Approved',
            'denied' => 'Denied',
            'expired' => 'Expired: the window ended before it was decided',
            default => Json::string($approval['status'] ?? null, 'Unknown'),
        };
    }

    /**
     * What happens or happened. A decision and the follow-up's execution are separate events.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function outcome(array $approval): string
    {
        $followup = Json::string($approval['followup_job_id'] ?? null);

        return match ($approval['status'] ?? null) {
            'needs_owner' => self::ended($approval)
                ? 'Nothing was submitted. The Director no longer accepts an approval.'
                : 'Waiting on you. Nothing has been submitted.',
            'approved' => 'Approved for one submission.'.self::result($approval).($followup !== ''
                ? ' Follow-up job '.$followup.' carries it out in the same task; its result shows there, separately.'
                : ' The follow-up that carries it out shows in the same task, separately.'),
            'denied' => 'Denied. Nothing was submitted.',
            'expired' => 'Expired. Nothing was submitted.',
            default => '',
        };
    }

    /**
     * What the site did with an approved submission. The approval record's outcome is authoritative; the follow-up
     * job's state is not. There is no client re-propose route, so nothing here offers to approve again.
     *
     * @param  array<string, mixed>  $approval
     */
    private static function result(array $approval): string
    {
        $outcome = Json::record($approval['outcome'] ?? null);
        $reason = Json::string($outcome['reason'] ?? null);

        return match ($outcome['status'] ?? null) {
            'submitted', 'succeeded' => ' The site accepted the submission.',
            'submission_unknown' => ' The site received the submission, but the result is unknown. Check the site before trying again.',
            'refused' => ' The browser refused before sending anything'.($reason !== '' ? ': '.$reason : '').'. It will be proposed again on the next run.',
            default => '',
        };
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    private static function expiresAt(array $approval): ?CarbonImmutable
    {
        $at = $approval['expires_at'] ?? null;
        if (! is_string($at) || $at === '') {
            return null;
        }

        try {
            return CarbonImmutable::parse($at);
        } catch (Throwable) {
            return null;
        }
    }
}
