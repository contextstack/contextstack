<?php

use App\Client\Bundle;

it('lists files sorted by path with their sha256 and mode', function () {
    $bundle = (new Bundle)->add('tasks/t/goal.md', "Say done.\n")->add('spec.yaml', "id: x\n")->add('tasks/t/check', "true\n", Bundle::Executable);

    expect($bundle->files())->toBe([
        ['path' => 'spec.yaml', 'sha256' => hash('sha256', "id: x\n"), 'mode' => 420],
        ['path' => 'tasks/t/check', 'sha256' => hash('sha256', "true\n"), 'mode' => 493],
        ['path' => 'tasks/t/goal.md', 'sha256' => hash('sha256', "Say done.\n"), 'mode' => 420],
    ]);
});

it('refuses a path that is not plain and relative', function (string $path) {
    (new Bundle)->add($path, 'x');
})->throws(InvalidArgumentException::class)->with(['/etc/passwd', '../up', 'a/../b', 'a//b', './a', '', "a\x00b"]);

it('reads a zip under its own paths, dropping one top-level folder and keeping the executable bit', function () {
    $path = tempnam(sys_get_temp_dir(), 'bundle').'.zip';
    $zip = new ZipArchive;
    $zip->open($path, ZipArchive::CREATE);
    $zip->addFromString('mybundle/spec.yaml', "id: z\n");
    $zip->addFromString('mybundle/tasks/t/check', "#!/bin/sh\ntrue\n");
    $zip->setExternalAttributesName('mybundle/tasks/t/check', ZipArchive::OPSYS_UNIX, 0100755 << 16);
    $zip->addFromString('mybundle/tasks/t/goal.md', "Go.\n");
    $zip->setExternalAttributesName('mybundle/tasks/t/goal.md', ZipArchive::OPSYS_UNIX, 0100644 << 16);
    $zip->addFromString('mybundle/link', 'target');
    $zip->setExternalAttributesName('mybundle/link', ZipArchive::OPSYS_UNIX, 0120777 << 16);
    $zip->close();

    $files = collect((new Bundle)->addZip($path)->files())->keyBy('path');
    unlink($path);

    expect($files->keys()->all())->toBe(['spec.yaml', 'tasks/t/check', 'tasks/t/goal.md'])
        ->and($files['tasks/t/check']['mode'])->toBe(493)
        ->and($files['tasks/t/goal.md']['mode'])->toBe(420);
});
