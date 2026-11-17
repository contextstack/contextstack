<?php

namespace App\Client;

use Psr\Http\Message\StreamInterface;
use RuntimeException;

/**
 * A response sink that hands every chunk on as it arrives instead of keeping it: the event-stream relay writes
 * each chunk to the browser and flushes. Returning 0 from write() makes the HTTP client stop the transfer, which
 * is how a closed browser connection ends the relay.
 *
 * Only a 200 answer is handed on; until the status is known to be 200 (a refusal, say), chunks are held so the
 * caller can read the refusal.
 */
final class PassThroughStream implements StreamInterface
{
    /** The answer's status, once its headers arrive. */
    public ?int $status = null;

    private int $written = 0;

    private string $held = '';

    private bool $forwarded = false;

    /** Whether what was handed on so far ends between two events, where a comment line may go. */
    private bool $atBoundary = true;

    private float $lastWrite = 0.0;

    /**
     * @param  callable(string): bool  $forward  hands a chunk on; false when the receiver is gone
     */
    public function __construct(private $forward) {}

    public function write(string $string): int
    {
        if ($this->status !== 200) {
            $this->held .= $string;
        } else {
            $this->forwarded = true;
            if (! ($this->forward)($string)) {
                return 0;
            }
            $this->atBoundary = str_ends_with($string, "\n\n");
            $this->lastWrite = microtime(true);
        }
        $this->written += strlen($string);

        return strlen($string);
    }

    /**
     * Whether the receiver is still there, asked while the answer is idle. PHP learns that a browser left only when
     * it next writes, and an idle Director stream writes once every 15 s, so a page you navigated away from would
     * hold its PHP worker that long (the desktop app has eight). After a second with nothing written, this writes an
     * SSE comment line, which the browser ignores, but only between two events, never inside one.
     */
    public function stillWanted(float $idleSeconds = 1.0): bool
    {
        if ($this->status !== 200 || ! $this->atBoundary || microtime(true) - $this->lastWrite < $idleSeconds) {
            return true;
        }
        $this->lastWrite = microtime(true);

        return (bool) ($this->forward)(":\n\n");
    }

    /**
     * What was held back: the body of a refusal, or of an answer whose status came only at the end.
     */
    public function held(): string
    {
        return $this->held;
    }

    public function forwardedAny(): bool
    {
        return $this->forwarded;
    }

    public function __toString(): string
    {
        return '';
    }

    public function close(): void {}

    public function detach()
    {
        return null;
    }

    public function getSize(): ?int
    {
        return null;
    }

    public function tell(): int
    {
        return $this->written;
    }

    public function eof(): bool
    {
        return true;
    }

    public function isSeekable(): bool
    {
        return false;
    }

    public function seek(int $offset, int $whence = SEEK_SET): void
    {
        throw new RuntimeException('A pass-through stream cannot seek.');
    }

    public function rewind(): void
    {
        throw new RuntimeException('A pass-through stream cannot rewind.');
    }

    public function isWritable(): bool
    {
        return true;
    }

    public function isReadable(): bool
    {
        return false;
    }

    public function read(int $length): string
    {
        throw new RuntimeException('A pass-through stream keeps nothing to read.');
    }

    public function getContents(): string
    {
        return '';
    }

    public function getMetadata(?string $key = null): mixed
    {
        return $key === null ? [] : null;
    }
}
