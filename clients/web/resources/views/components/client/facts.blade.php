{{-- Label and value pairs, two columns: fill with <x-client.fact>. --}}
<dl {{ $attributes->class('grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-1.5 text-sm') }}>
    {{ $slot }}
</dl>
