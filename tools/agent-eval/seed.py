"""The fixture: a shop of customers, products, orders and their lines.

Made from a fixed seed, so the database every run starts from and the
answers `tasks.py` works out in Python are the same rows. Ids are given, 1
up in each collection, so a task can name them and an insert takes the next.
"""

import datetime as dt
import random

SCHEMA = [
    "create collection customers (name text, email text nocase @unique, "
    "country text @hash, city text, tier text, signup timestamp @sorted, meta json)",
    "create collection products (sku text @unique, name text, category text @hash, "
    "price int @sorted, stock int, discontinued bool, description text @text, tags [text])",
    "create collection orders (customer_id int @hash, status text @hash, "
    "placed timestamp @sorted, total int, discount int, note text)",
    "create collection order_lines (order_id int @hash, product_id int @hash, "
    "qty int, unit_price int)",
]

FIRST = ["Anna", "Ben", "Carla", "David", "Elif", "Felix", "Greta", "Hugo", "Ines",
         "Jonas", "Kerem", "Lena", "Marco", "Nora", "Oscar", "Paula", "Quentin",
         "Rosa", "Sven", "Tara"]
LAST = ["Adler", "Brandt", "Costa", "Dubois", "Eriksen", "Fischer", "Garcia",
        "Hoffmann", "Ivanova", "Jensen", "Klein", "Lopez", "Martin", "Novak",
        "Ozturk", "Peters", "Rossi", "Schmidt", "Toprak", "Visser"]
COUNTRIES = [("DE", 14), ("FR", 11), ("ES", 9), ("IT", 8), ("TR", 7), ("NL", 6), ("US", 5)]
CITIES = {"DE": ["Berlin", "Munich", "Hamburg"], "FR": ["Paris", "Lyon"],
          "ES": ["Madrid", "Barcelona"], "IT": ["Rome", "Milan"],
          "TR": ["Istanbul", "Ankara"], "NL": ["Amsterdam", "Utrecht"],
          "US": ["Boston", "Austin"]}
DOMAINS = ["example.com", "example.org", "mail.test"]

# Every product: (name, category, words of its description). A category's
# count decides which pass `having count(*) > 6` (lighting, kitchen, garden).
PRODUCTS = [
    ("Desk Lamp", "lighting", "adjustable arm, warm light"),
    ("Floor Lamp", "lighting", "tall reading light with a linen shade"),
    ("Clip Lamp", "lighting", "clips onto shelves, wireless charging base"),
    ("Ceiling Light", "lighting", "flush mount, dimmable"),
    ("String Lights", "lighting", "waterproof outdoor string of fifty bulbs"),
    ("Lamp Shade", "lighting", "linen shade for table lamps"),
    ("Night Light", "lighting", "soft glow, motion sensor"),
    ("Wall Sconce", "lighting", "brass finish, hardwired"),
    ("Chef Knife", "kitchen", "forged steel blade"),
    ("Cutting Board", "kitchen", "bamboo board with juice groove"),
    ("Spice Rack", "kitchen", "bamboo rack for twelve jars"),
    ("Mixing Bowls", "kitchen", "set of three stainless bowls"),
    ("Tea Kettle", "kitchen", "whistling kettle, induction ready"),
    ("Salad Spinner", "kitchen", "dries greens fast"),
    ("Coffee Grinder", "kitchen", "burr grinder, twelve settings"),
    ("Utensil Set", "kitchen", "silicone heads, steel handles"),
    ("Garden Hose", "garden", "fifty feet, kink resistant"),
    ("Pruning Shears", "garden", "bypass blades, soft grip"),
    ("Watering Can", "garden", "two gallons, long spout"),
    ("Planter Box", "garden", "cedar box, drainage holes"),
    ("Garden Gloves", "garden", "waterproof gloves, nitrile coated"),
    ("Seed Starter", "garden", "tray of seventy two cells"),
    ("Rain Gauge", "garden", "glass tube, steel stake"),
    ("Building Blocks", "toys", "two hundred pieces"),
    ("Puzzle Cube", "toys", "speed cube, smooth turning"),
    ("Lava Lamp", "toys", "retro glow for a desk"),
    ("Kite", "toys", "nylon diamond kite"),
    ("Toy Train", "toys", "wooden track and engine"),
    ("Water Blaster", "toys", "waterproof pump blaster for the pool"),
    ("Notebook", "office", "dotted pages, lay flat"),
    ("Desk Organizer", "office", "bamboo trays and a pen cup"),
    ("Stapler", "office", "full strip, jam free"),
    ("Monitor Stand", "office", "raises a screen four inches"),
    ("Wireless Mouse", "office", "wireless, silent clicks"),
    ("Paper Shredder", "office", "cross cut, eight sheets"),
    ("Camping Tent", "outdoor", "two person tent, waterproof fly"),
    ("Sleeping Bag", "outdoor", "rated to five degrees"),
    ("Hiking Poles", "outdoor", "carbon, folding"),
    ("Headlamp", "outdoor", "rechargeable, three hundred lumens"),
    ("Cooler Box", "outdoor", "keeps ice three days"),
]

NOTES = ["gift wrap please", "leave at the door", "gift for my mother",
         "call before delivery", "fragile"]


def iso(t):
    return t.strftime("%Y-%m-%dT%H:%M:%SZ")


def at(rng, start, end):
    """A whole second between two dates, as a UTC datetime."""
    a = dt.datetime.fromisoformat(start).replace(tzinfo=dt.timezone.utc)
    b = dt.datetime.fromisoformat(end).replace(tzinfo=dt.timezone.utc)
    return a + dt.timedelta(seconds=rng.randrange(int((b - a).total_seconds())))


def data():
    rng = random.Random(38)

    countries = [c for c, n in COUNTRIES for _ in range(n)]
    rng.shuffle(countries)
    names = [(f, l) for f in FIRST for l in LAST]
    rng.shuffle(names)
    customers = []
    for i, country in enumerate(countries, 1):
        first, last = names[i - 1]
        meta = None
        if rng.random() < 0.8:
            meta = {"source": rng.choice(["ads", "organic", "referral"]),
                    "newsletter": rng.random() < 0.5}
        customers.append({
            "id": i,
            "name": f"{first} {last}",
            "email": f"{first}.{last}@{rng.choice(DOMAINS)}".lower(),
            "country": country,
            "city": rng.choice(CITIES[country]),
            "tier": rng.choices(["gold", "silver", "bronze", None], [2, 3, 3, 2])[0],
            "signup": iso(at(rng, "2024-01-01", "2026-09-30")),
            "meta": meta,
        })

    # Prices are cents. 2000 and 5000 are held by a product each, so a range
    # "from 20.00 to 50.00" tells an inclusive bound from an exclusive one.
    prices = rng.sample(range(5, 150), len(PRODUCTS))
    products = []
    for i, ((name, category, words), p) in enumerate(zip(PRODUCTS, prices), 1):
        price = p * 100 - 1
        if name == "Spice Rack":
            price = 2000
        if name == "Planter Box":
            price = 5000
        products.append({
            "id": i,
            "sku": f"SKU-{1000 + i}",
            "name": name,
            "category": category,
            "price": price,
            "stock": rng.choice([0, 0, 3, 8, 12, 20, 35, 60]),
            "discontinued": rng.random() < 0.15,
            "description": f"{name}: {words}.",
            "tags": rng.sample(["new", "sale", "eco", "bestseller", "gift"], rng.randint(0, 2)),
        })

    # Eight customers order nothing, for "never placed an order".
    buyers = [c["id"] for c in customers]
    rng.shuffle(buyers)
    idle = set(buyers[:8])
    buyers = [c for c in buyers if c not in idle]
    orders, lines = [], []
    for i in range(1, 201):
        # A few customers order often, so "their last three orders" has more.
        customer = buyers[0] if rng.random() < 0.06 else rng.choice(buyers)
        status = rng.choices(["paid", "shipped", "pending", "cancelled"], [45, 30, 15, 10])[0]
        discount = rng.choice([200, 500, 1000]) if rng.random() < 0.2 else None
        note = rng.choice(NOTES) if rng.random() < 0.3 else None
        chosen = rng.sample(products, rng.randint(1, 4))
        total = 0
        for prod in chosen:
            qty = rng.randint(1, 5)
            lines.append({"id": len(lines) + 1, "order_id": i, "product_id": prod["id"],
                          "qty": qty, "unit_price": prod["price"]})
            total += qty * prod["price"]
        orders.append({
            "id": i,
            "customer_id": customer,
            "status": status,
            "placed": iso(at(rng, "2025-06-01", "2026-09-30")),
            "total": total - (discount or 0),
            "discount": discount,
            "note": note,
        })
    return {"customers": customers, "products": products, "orders": orders,
            "order_lines": lines}
