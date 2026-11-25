<?php

declare(strict_types=1);

namespace App\Client;

/**
 * Summaries from a run's evidence files (cs_runs.py).
 */
final class RunEvidence
{
    /**
     * @return array{files: list<string>, added: int, removed: int}
     */
    public static function diffStat(string $raw): array
    {
        $files = [];
        $plus = 0;
        $minus = 0;
        // Lines still to come in the current hunk (from its `@@ -a,old +b,new @@` header): inside a hunk, a line
        // starting `--- ` or `+++ ` is a removed `-- ` or added `++ ` line, not a file header.
        $old = 0;
        $new = 0;
        foreach (self::lines($raw) as $line) {
            if (($old > 0 || $new > 0) && ! str_starts_with($line, 'diff ')) {
                if (str_starts_with($line, '+')) {
                    $new--;
                    $plus++;
                } elseif (str_starts_with($line, '-')) {
                    $old--;
                    $minus++;
                } elseif (! str_starts_with($line, '\\')) {
                    $old--;
                    $new--;
                }

                continue;
            }
            $old = 0;
            $new = 0;
            if (preg_match('/^@@ -\d+(?:,(\d+))? \+\d+(?:,(\d+))? @@/', $line, $hunk) === 1) {
                $old = ($hunk[1] ?? '') === '' ? 1 : (int) $hunk[1];
                $new = ($hunk[2] ?? '') === '' ? 1 : (int) $hunk[2];
            } elseif (str_starts_with($line, '+++ ') || str_starts_with($line, '--- ')) {
                $name = explode("\t", substr($line, 4))[0];
                if ($name !== '/dev/null') {
                    $name = preg_replace('/^[ab]\//', '', $name) ?? $name;
                    $files[$name] = $name;
                }
            } elseif (str_starts_with($line, '+')) {
                $plus++;
            } elseif (str_starts_with($line, '-')) {
                $minus++;
            }
        }
        $listed = array_values($files);
        sort($listed);

        return ['files' => array_slice($listed, 0, 50), 'added' => $plus, 'removed' => $minus];
    }

    /**
     * The agent's last message from a stream-json transcript: its closing report. Earlier messages are progress notes,
     * and a CLI's result event may repeat all of them run together (Cursor's does), so neither is shown. Falls back to
     * the result event, then to the raw text. A message longer than the limit keeps its end, cut at a line break.
     */
    public static function finalMessage(string $raw, int $limit = 4000): string
    {
        $last = null;
        $result = null;
        foreach (self::lines($raw) as $line) {
            if ($line === '') {
                continue;
            }
            try {
                /** @var array<string, mixed> $event */
                $event = json_decode($line, true, 512, JSON_THROW_ON_ERROR);
            } catch (\JsonException) {
                continue;
            }
            if (($event['type'] ?? null) === 'assistant') {
                $texts = [];
                foreach ((array) (((array) ($event['message'] ?? []))['content'] ?? []) as $block) {
                    if (is_array($block) && ($block['type'] ?? null) === 'text' && trim((string) ($block['text'] ?? '')) !== '') {
                        $texts[] = trim((string) $block['text']);
                    }
                }
                if ($texts !== []) {
                    $last = implode("\n\n", $texts);
                }
            } elseif (($event['type'] ?? null) === 'result' && filled($event['result'] ?? null)) {
                $result = trim((string) $event['result']);
            }
        }
        $text = $last ?? $result ?? trim($raw);
        if (mb_strlen($text) <= $limit) {
            return $text;
        }
        $kept = mb_substr($text, -$limit);
        $break = mb_strpos($kept, "\n");

        return '…'.($break !== false && $break < $limit / 2 ? mb_substr($kept, $break + 1) : $kept);
    }

    /**
     * @return list<string>
     */
    private static function lines(string $raw): array
    {
        return explode("\n", str_replace(["\r\n", "\r"], "\n", $raw));
    }
}
