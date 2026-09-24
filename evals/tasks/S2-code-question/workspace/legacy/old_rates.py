"""DEPRECATED 2024: kept for the old invoice exporter only. Not used by checkout."""


def estimate_shipping(weight_kg, zone, tier="standard"):
    per_kg = {"A": 40, "B": 55, "C": 70}[zone]
    fee = 50 + per_kg * weight_kg
    if tier == "gold":
        fee *= 0.85
    return round(fee)
