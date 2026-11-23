{{-- The app's tabs, shared by every top-level screen. --}}
<native:bottom-nav>
    <native:bottom-nav-item id="waiting" label="Waiting" url="/" icon="bell" badge="{{ ($waitingCount ?? 0) ?: '' }}" />
    <native:bottom-nav-item id="projects" label="Projects" url="/projects" icon="folder" />
    <native:bottom-nav-item id="settings" label="Settings" url="/settings" icon="settings" />
</native:bottom-nav>
