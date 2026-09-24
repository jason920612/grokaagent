"""Delivery zones and their price multipliers."""

ZONE_MULTIPLIER = {
    "A": 1.0,   # same city
    "B": 1.35,  # same island
    "C": 1.8,   # outlying islands
}


def multiplier(zone: str) -> float:
    try:
        return ZONE_MULTIPLIER[zone.upper()]
    except KeyError:
        raise ValueError(f"unknown zone {zone!r}") from None
