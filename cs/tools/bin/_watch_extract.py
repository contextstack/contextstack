"""Deterministic price extraction inside the browser_session sandbox."""
import math
import re


def extract_price(text, extract, *, selected=None):
    """Read the first price from the first selector match or the regex match."""
    if "regex" in extract:
        match = re.search(extract["regex"], text)
        candidate = match.group(1 if match.re.groups else 0) if match else None
    else:
        candidate = selected
    decimal, grouping = (".", ",") if extract["number_parser"] == "decimal_dot" else (",", ".")
    number = re.search(r"(?<![0-9.,+-])[0-9]+(?:[.,][0-9]+)*", candidate or "")
    if number:
        whole, separator, fraction = number.group().partition(decimal)
        groups = whole.split(grouping)
        grouped = len(groups) == 1 or (len(groups[0]) <= 3 and all(len(group) == 3 for group in groups[1:]))
        if grouped and (not separator or fraction.isascii() and fraction.isdecimal()):
            price = float("".join(groups) + ("." + fraction if separator else ""))
            if math.isfinite(price):
                return price
    return None
