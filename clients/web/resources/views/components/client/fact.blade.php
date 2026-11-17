@props(['label', 'mono' => false])

{{-- One label and value. Identifiers (mono) stay on one line and truncate, with the whole value as a title; prose wraps. --}}
<dt class="text-zinc-500">{{ $label }}</dt>
<dd {{ $attributes->class(['min-w-0', 'truncate font-mono text-xs leading-5' => $mono]) }}>{{ $slot }}</dd>
