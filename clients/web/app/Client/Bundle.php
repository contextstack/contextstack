<?php

namespace App\Client;

use InvalidArgumentException;
use ZipArchive;

/**
 * An intent's bundle as the client supplies it (CLIENT-API.md §4.1): content-addressed files, each `{path, sha256,
 * mode}`, mode 420 (0644) or 493 (0755). Paths are plain and relative, and each appears once.
 */
final class Bundle
{
    public const int Regular = 420;

    public const int Executable = 493;

    public const int MaxFiles = 2000;

    /** @var array<string, array{bytes: string, mode: int}> */
    private array $entries = [];

    public function add(string $path, string $bytes, int $mode = self::Regular): self
    {
        $path = self::plainPath($path);

        if (! in_array($mode, [self::Regular, self::Executable], true)) {
            throw new InvalidArgumentException("{$path}: mode must be 0644 or 0755.");
        }

        if (count($this->entries) >= self::MaxFiles && ! isset($this->entries[$path])) {
            throw new InvalidArgumentException('A bundle holds at most '.self::MaxFiles.' files.');
        }

        $this->entries[$path] = ['bytes' => $bytes, 'mode' => $mode];

        return $this;
    }

    /**
     * Add every regular file of a zip archive, under its own paths. A file whose archived Unix mode is executable
     * is offered as 0755. A single top-level folder holding everything is dropped, so zipping a bundle's folder
     * gives the bundle's own paths.
     */
    public function addZip(string $archivePath): self
    {
        $zip = new ZipArchive;

        if ($zip->open($archivePath, ZipArchive::RDONLY) !== true) {
            throw new InvalidArgumentException('The archive could not be read as a zip file.');
        }

        try {
            $found = [];
            for ($index = 0; $index < $zip->numFiles; $index++) {
                $name = (string) $zip->getNameIndex($index);
                if ($name === '' || str_ends_with($name, '/') || str_starts_with($name, '__MACOSX/') || basename($name) === '.DS_Store') {
                    continue;
                }
                $opsys = 0;
                $attributes = 0;
                $zip->getExternalAttributesIndex($index, $opsys, $attributes);
                $unixMode = $opsys === ZipArchive::OPSYS_UNIX ? ($attributes >> 16) & 0o170777 : 0;
                if ($unixMode !== 0 && ($unixMode & 0o170000) !== 0o100000) {
                    continue;
                }
                $found[$name] = ['index' => $index, 'mode' => ($unixMode & 0o111) !== 0 ? self::Executable : self::Regular];
            }

            $strip = self::commonFolder(array_keys($found));
            foreach ($found as $name => $entry) {
                $bytes = $zip->getFromIndex($entry['index']);
                if ($bytes === false) {
                    throw new InvalidArgumentException("{$name} could not be read from the archive.");
                }
                $this->add(substr($name, strlen($strip)), $bytes, $entry['mode']);
            }
        } finally {
            $zip->close();
        }

        return $this;
    }

    public function has(string $path): bool
    {
        return isset($this->entries[$path]);
    }

    public function isEmpty(): bool
    {
        return $this->entries === [];
    }

    /**
     * The list an intent sends, sorted by path.
     *
     * @return list<array{path: string, sha256: string, mode: int}>
     */
    public function files(): array
    {
        $files = [];
        foreach ($this->entries as $path => $entry) {
            $files[] = ['path' => $path, 'sha256' => hash('sha256', $entry['bytes']), 'mode' => $entry['mode']];
        }
        usort($files, fn (array $a, array $b): int => strcmp($a['path'], $b['path']));

        return $files;
    }

    /**
     * Each distinct object's bytes, by sha256.
     *
     * @return array<string, string>
     */
    public function contents(): array
    {
        $contents = [];
        foreach ($this->entries as $entry) {
            $contents[hash('sha256', $entry['bytes'])] = $entry['bytes'];
        }

        return $contents;
    }

    public static function plainPath(string $path): string
    {
        $path = str_replace('\\', '/', trim($path));
        $segments = explode('/', $path);

        if ($path === '' || str_starts_with($path, '/') || in_array('', $segments, true) || in_array('.', $segments, true) || in_array('..', $segments, true) || preg_match('/[\x00-\x1f]/', $path) === 1) {
            throw new InvalidArgumentException("\"{$path}\" is not a plain relative path.");
        }

        return $path;
    }

    /**
     * @param  list<string>  $names
     */
    private static function commonFolder(array $names): string
    {
        if ($names === []) {
            return '';
        }

        $first = explode('/', $names[0])[0].'/';
        foreach ($names as $name) {
            if (! str_starts_with($name, $first)) {
                return '';
            }
        }

        return $first;
    }
}
