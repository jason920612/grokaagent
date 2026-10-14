"""Compatibility layer imported by the external billing exporter.

FROZEN: the billing exporter calls these functions directly. Their names,
signatures and behaviour (including returning None for a missing note) must
not change.
"""


def get_note(store, note_id):
    """The note dict, or None when it does not exist."""
    return store.get(note_id)


def list_notes(store):
    """All notes, oldest first, as dicts with id/title/body/created."""
    return sorted(store.all(), key=lambda n: n["id"])


def format_note(note):
    """One line per note: '#<id> <title>'."""
    return f"#{note['id']} {note['title']}"
