<?php

declare(strict_types=1);

namespace App\Client;

/**
 * How a failed conversation turn reads: the Director's own explanation (`failure` on the turn, CLIENT-API.md), so every
 * client says the same. `failure.message` is the plain sentence; `failure.model_switch_may_help` says whether to offer
 * another model; `failure.detail` (when there is one) is the machine's specific cause, shown under it. A turn without one (recorded before the Director explained failures) falls back to its detail.
 */
final class TurnFailure
{
    /**
     * @param  array<string, mixed>  $turn
     * @return array{code: string, message: string, detail: string|null, model_switch_may_help: bool}|null
     */
    public static function of(array $turn): ?array
    {
        $failure = $turn['failure'] ?? null;
        if (! is_array($failure) || ! is_string($failure['code'] ?? null)) {
            return null;
        }

        return [
            'code' => $failure['code'],
            'message' => is_string($failure['message'] ?? null) ? trim($failure['message']) : '',
            'detail' => is_string($failure['detail'] ?? null) && trim($failure['detail']) !== '' ? trim($failure['detail']) : null,
            'model_switch_may_help' => ($failure['model_switch_may_help'] ?? false) === true,
        ];
    }

    /**
     * @param  array<string, mixed>  $turn
     */
    public static function offersModelSwitch(array $turn): bool
    {
        return self::of($turn)['model_switch_may_help'] ?? false;
    }

    /**
     * The specific cause, when the Director has one beyond its sentence.
     *
     * @param  array<string, mixed>  $turn
     */
    public static function detail(array $turn): ?string
    {
        $failure = self::of($turn);

        return $failure !== null && $failure['detail'] !== $failure['message'] ? $failure['detail'] : null;
    }

    /**
     * @param  array<string, mixed>  $turn
     */
    public static function plainReason(array $turn): string
    {
        $message = self::of($turn)['message'] ?? '';
        if ($message !== '') {
            return $message;
        }

        return filled($turn['detail'] ?? null) ? (string) $turn['detail'] : __('It failed without a reply.');
    }
}
