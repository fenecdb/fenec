"""The tasks, each with its answer worked out in Python over seed.py's rows.

A read's answer is rows of values: the last statement the agent ran that
succeeded must answer as many rows, each holding those values (other
columns may come beside them). A write is checked by the queries in its
`verify`, which the harness runs after the agent is done; one with an
`answer` must also have had some statement answer those rows (the id it
made, the rows it deleted). Nothing here asks the server: a fixture that
moved would show as tasks failing in every condition, not as answers that
followed it.
"""

import datetime as dt

import seed


def ts(s):
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00"))


def between(t, a, b):
    """`a <= t < b`, three ISO texts."""
    return ts(a) <= ts(t) < ts(b)


def unique_top(rows, key, n=1):
    """The first `n` rows by `key` descending, held to have no tie at the cut."""
    rows = sorted(rows, key=key, reverse=True)
    assert len(rows) <= n or key(rows[n - 1]) != key(rows[n]), "a tie at the cut"
    return rows[:n]


def tasks():
    d = seed.data()
    C, P, O, L = d["customers"], d["products"], d["orders"], d["order_lines"]
    by_customer = {}
    for o in O:
        by_customer.setdefault(o["customer_id"], []).append(o)
    prod = {p["id"]: p for p in P}
    out = []

    def read(id, area, prompt, rows, ordered=False):
        out.append({"id": id, "area": area, "kind": "read", "prompt": prompt,
                    "expect": rows, "ordered": ordered})

    def write(id, area, prompt, verify, answer=None):
        out.append({"id": id, "area": area, "kind": "write", "prompt": prompt,
                    "verify": verify, "answer": answer})

    def check(query, rows, ordered=False):
        return {"query": query, "rows": rows, "ordered": ordered}

    # Filters, ranges, IN.
    read("T01", "filter", "How many customers live in Germany (country code DE)?",
         [[sum(c["country"] == "DE" for c in C)]])
    read("T02", "filter",
         "List the names of the customers from France (FR) or Spain (ES) who signed up during 2025 (UTC).",
         [[c["name"]] for c in C if c["country"] in ("FR", "ES")
          and between(c["signup"], "2025-01-01T00:00:00Z", "2026-01-01T00:00:00Z")])
    read("T03", "filter",
         "Which products cost from 20.00 to 50.00, both ends included? Prices are stored in cents. Return their SKUs.",
         [[p["sku"]] for p in P if 2000 <= p["price"] <= 5000])
    read("T04", "filter",
         "Which products have \"lamp\" in their name, in any letter case? Return their names.",
         [[p["name"]] for p in P if "lamp" in p["name"].lower()])
    read("T05", "filter",
         "Give the ids of the orders whose status is pending or cancelled and that were placed in March 2026 (UTC).",
         [[o["id"]] for o in O if o["status"] in ("pending", "cancelled")
          and between(o["placed"], "2026-03-01T00:00:00Z", "2026-04-01T00:00:00Z")])
    read("T06", "filter",
         "Which customers have an email address at the domain example.org? Return their emails.",
         [[c["email"]] for c in C if c["email"].endswith("@example.org")])
    read("T07", "filter",
         "List the SKUs of the products that are in stock (stock above 0) and whose category is neither toys nor garden.",
         [[p["sku"]] for p in P if p["stock"] > 0 and p["category"] not in ("toys", "garden")])

    # Order and pages.
    top5 = unique_top(P, lambda p: p["price"], 5)
    read("T08", "order",
         "What are the 5 most expensive products? Return each one's name and price, the most expensive first.",
         [[p["name"], p["price"]] for p in top5], ordered=True)
    by_signup = sorted(C, key=lambda c: ts(c["signup"]))
    read("T09", "order",
         "List the customers by signup time, oldest first, 10 to a page. Return the names on page 3.",
         [[c["name"]] for c in by_signup[20:30]], ordered=True)
    often = max(by_customer, key=lambda k: len(by_customer[k]))
    often_email = next(c["email"] for c in C if c["id"] == often)
    recent = sorted(by_customer[often], key=lambda o: ts(o["placed"]), reverse=True)
    assert len(recent) >= 4
    read("T10", "order",
         f"Which are the 3 most recently placed orders of the customer whose email is {often_email}? "
         "Return their ids, newest first.",
         [[o["id"]] for o in recent[:3]], ordered=True)

    # Aggregates.
    read("T11", "aggregate",
         "What is the sum of the totals, in cents, of all orders whose status is paid or shipped?",
         [[sum(o["total"] for o in O if o["status"] in ("paid", "shipped"))]])
    statuses = {}
    for o in O:
        statuses[o["status"]] = statuses.get(o["status"], 0) + 1
    read("T12", "aggregate",
         "How many orders are there in each status? Return each status with its count.",
         [[s, n] for s, n in statuses.items()])
    cats = {}
    for p in P:
        cats.setdefault(p["category"], []).append(p["price"])
    read("T13", "aggregate",
         "For each product category that has more than 6 products, what is the average price in cents? "
         "Return the category and the average.",
         [[c, sum(v) / len(v)] for c, v in cats.items() if len(v) > 6])
    read("T14", "aggregate",
         "How many different customers placed at least one order in 2026 (UTC)?",
         [[len({o["customer_id"] for o in O
                if between(o["placed"], "2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z")})]])
    per_country = {}
    for c in C:
        per_country[c["country"]] = per_country.get(c["country"], 0) + 1
    big = sorted([(n, k) for k, n in per_country.items() if n >= 8], reverse=True)
    assert len({n for n, _ in big}) == len(big)
    read("T15", "aggregate",
         "Which countries have at least 8 customers? Return each country with its number of customers, the largest first.",
         [[k, n] for n, k in big], ordered=True)
    read("T16", "aggregate",
         "What are the largest and the smallest order totals, in cents? Answer both in one result.",
         [[max(o["total"] for o in O), min(o["total"] for o in O)]])
    spend = {}
    for o in O:
        if o["status"] != "cancelled":
            spend[o["customer_id"]] = spend.get(o["customer_id"], 0) + o["total"]
    best = unique_top(list(spend.items()), lambda kv: kv[1])[0]
    read("T17", "aggregate",
         "Which customer has spent the most, counting only orders that were not cancelled? "
         "Return the customer's id and the amount in cents.",
         [[best[0], best[1]]])
    read("T18", "aggregate",
         "List each distinct city that customers in Germany (DE) live in, once.",
         [[city] for city in sorted({c["city"] for c in C if c["country"] == "DE"})])

    # Parents and children.
    cancelled_by = {o["customer_id"] for o in O if o["status"] == "cancelled"}
    read("T19", "related",
         "Which customers have at least one cancelled order? Return their names, each once.",
         [[c["name"]] for c in C if c["id"] in cancelled_by])
    three = next(o for o in O if sum(l["order_id"] == o["id"] for l in L) == 3)
    read("T20", "related",
         f"For order {three['id']}, list each of its lines' product name and quantity.",
         [[prod[l["product_id"]]["name"], l["qty"]] for l in L if l["order_id"] == three["id"]])
    read("T21", "related",
         "Which customers have never placed an order? Return their emails.",
         [[c["email"]] for c in C if c["id"] not in by_customer])
    read("T22", "related",
         "What is the total quantity sold, over all order lines, of products in the lighting category?",
         [[sum(l["qty"] for l in L if prod[l["product_id"]]["category"] == "lighting")]])
    read("T23", "related",
         "For each customer from Italy (IT) who has placed at least one order, return the customer's id "
         "and their number of orders.",
         [[c["id"], len(by_customer[c["id"]])] for c in C
          if c["country"] == "IT" and c["id"] in by_customer])

    # Text.
    read("T24", "text",
         "Which products mention the word \"waterproof\" in their description? Return their SKUs.",
         [[p["sku"]] for p in P if "waterproof" in p["description"].lower()])
    read("T25", "text",
         "Which products in the kitchen category mention \"bamboo\" in their description? Return their names.",
         [[p["name"]] for p in P if p["category"] == "kitchen" and "bamboo" in p["description"].lower()])

    # Nulls.
    read("T26", "null", "How many customers have no tier?",
         [[sum(c["tier"] is None for c in C)]])
    read("T27", "null",
         "How many customers have a tier other than gold? Customers with no tier at all do not count.",
         [[sum(c["tier"] is not None and c["tier"] != "gold" for c in C)]])
    disc = [o["discount"] for o in O if o["discount"] is not None]
    read("T28", "null",
         "What is the average discount, in cents, over the orders that have a discount?",
         [[sum(disc) / len(disc)]])
    read("T29", "null",
         "What is the average discount per order, in cents, over all orders, counting an order without a discount as 0?",
         [[sum(disc) / len(O)]])
    read("T30", "null",
         "How many customers came from ads, going by the source recorded in their meta field?",
         [[sum((c["meta"] or {}).get("source") == "ads" for c in C)]])

    # Writes.
    write("T31", "insert",
          "Add a customer: name Zeynep Kaya, email zeynep.kaya@example.com, country TR, city Izmir, "
          "signed up 2026-10-01T09:00:00Z. Leave the tier empty.",
          [check('get customers select name, email, country, city, tier, signup where email = "zeynep.kaya@example.com"',
                 [["Zeynep Kaya", "zeynep.kaya@example.com", "TR", "Izmir", None, "2026-10-01T09:00:00Z"]]),
           check("get customers count", [[len(C) + 1]])])
    write("T32", "insert",
          "Add two products in the office category, in one statement if you can: SKU-2001 \"Cable Tray\" "
          "priced 24.50 with stock 30, and SKU-2002 \"Monitor Arm\" priced 89.00 with stock 12. "
          "Prices are stored in cents.",
          [check('get products select sku, name, category, price, stock where sku in ["SKU-2001", "SKU-2002"]',
                 [["SKU-2001", "Cable Tray", "office", 2450, 30], ["SKU-2002", "Monitor Arm", "office", 8900, 12]]),
           check("get products count", [[len(P) + 2]])])
    write("T33", "update",
          "Add 10 to the stock of every product in the garden category.",
          [check("get products select sku, stock", [[p["sku"], p["stock"] + (10 if p["category"] == "garden" else 0)]
                                                     for p in P])])
    def cancel(o):
        return o["status"] == "pending" and ts(o["placed"]) < ts("2026-01-01T00:00:00Z")
    write("T34", "update",
          "Cancel every order that is still pending and was placed before 2026-01-01 (UTC): set its status to cancelled.",
          [check("get orders select id, status",
                 [[o["id"], "cancelled" if cancel(o) else o["status"]] for o in O])])
    gone = {o["id"] for o in O if o["status"] == "cancelled" and o["total"] < 5000}
    assert gone
    write("T35", "delete",
          "Delete the cancelled orders whose total is below 50.00 (5000 cents).",
          [check("get orders select id", [[o["id"]] for o in O if o["id"] not in gone])])
    s3 = next(p for p in P if p["sku"] == "SKU-1003")
    upsert = ("Stock arrived: 5 units of {sku}. If a product with that SKU exists, add 5 to its stock; "
              "if none does, create it with name \"Spare Part\", category misc, price 1000 cents and stock 5.")
    write("T36", "upsert", upsert.format(sku="SKU-1003"),
          [check('get products select sku, name, stock where sku = "SKU-1003"',
                 [["SKU-1003", s3["name"], s3["stock"] + 5]]),
           check("get products count", [[len(P)]])])
    write("T37", "upsert", upsert.format(sku="SKU-1099"),
          [check('get products select sku, name, category, price, stock where sku = "SKU-1099"',
                 [["SKU-1099", "Spare Part", "misc", 1000, 5]]),
           check("get products count", [[len(P) + 1]])])
    pending = sorted((o for o in O if o["status"] == "pending"), key=lambda o: ts(o["placed"]))
    oldest = pending[0]
    write("T38", "returning",
          "Mark the oldest pending order (the one placed first) as paid, and tell me its id.",
          [check("get orders select id, status",
                 [[o["id"], "paid" if o["id"] == oldest["id"] else o["status"]] for o in O])],
          answer=[[oldest["id"]]])
    assert not any(o["customer_id"] == 7 and o["total"] == 4999 for o in O)
    write("T39", "returning",
          "Create an order for customer 7: status pending, total 4999 cents, placed 2026-10-15T12:00:00Z. "
          "What id did the new order get?",
          [check("get orders select customer_id, status, total, placed where customer_id = 7 and total = 4999",
                 [[7, "pending", 4999, "2026-10-15T12:00:00Z"]]),
           check("get orders count", [[len(O) + 1]])],
          answer=[[len(O) + 1]])
    most = max(by_customer, key=lambda k: (sum(o["status"] == "cancelled" for o in by_customer[k]), -k))
    theirs = [o["id"] for o in by_customer[most] if o["status"] == "cancelled"]
    assert len(theirs) >= 2
    most_email = next(c["email"] for c in C if c["id"] == most)
    write("T40", "returning",
          f"Delete the cancelled orders of the customer whose email is {most_email}, and give me the ids of "
          "the orders you deleted.",
          [check("get orders select id", [[o["id"]] for o in O if o["id"] not in theirs])],
          answer=[[i] for i in theirs])

    # One FenecQL answer to each task, which `eval.py check` runs through the
    # scoring an agent's statements meet: what the tasks ask can be said, and
    # the answers above are the engine's.
    upsert_ref = ('put products {{sku: "{sku}", name: "Spare Part", category: "misc", price: 1000, '
                  'stock: 5}} if absent else set {{stock: stock + new.stock}}')
    ref = {
        "T01": 'get customers where country = "DE" count',
        "T02": 'get customers select name where country in ["FR", "ES"] and signup >= "2025-01-01" '
               'and signup < "2026-01-01"',
        "T03": "get products select sku where price >= 2000 and price <= 5000",
        "T04": 'get products select name where name ~ "lamp"',
        "T05": 'get orders select id where status in ["pending", "cancelled"] and placed >= "2026-03-01" '
               'and placed < "2026-04-01"',
        "T06": 'get customers select email where email ~ "@example.org"',
        "T07": 'get products select sku where stock > 0 and not (category in ["toys", "garden"])',
        "T08": "get products select name, price order price desc limit 5",
        "T09": "get customers select name order signup limit 10 offset 20",
        "T10": f'get orders select id where customer_id in (get customers select id where email = "{often_email}") '
               "order placed desc limit 3",
        "T11": 'get orders select sum(total) where status in ["paid", "shipped"]',
        "T12": "get orders select status, count(*) group status",
        "T13": "get products select category, avg(price) group category having count(*) > 6",
        "T14": 'get orders select count(distinct customer_id) where placed >= "2026-01-01" and placed < "2027-01-01"',
        "T15": "get customers select country, count(*) as n group country having n >= 8 order n desc",
        "T16": "get orders select max(total), min(total)",
        "T17": 'get orders select customer_id, sum(total) as spent where status != "cancelled" '
               "group customer_id order spent desc limit 1",
        "T18": 'get customers select city, count(*) where country = "DE" group city',
        "T19": 'get customers select name where id in (get orders select customer_id where status = "cancelled")',
        "T20": f"get order_lines select qty where order_id = {three['id']} lookup products on id = product_id select name",
        "T21": "get customers select email where not (id in (get orders select customer_id))",
        "T22": 'get order_lines select sum(qty) where product_id in (get products select id where category = "lighting")',
        "T23": 'get orders select customer_id, count(*) where customer_id in '
               '(get customers select id where country = "IT") group customer_id',
        "T24": 'get products select sku match description "waterproof"',
        "T25": 'get products select name where category = "kitchen" match description "bamboo"',
        "T26": "get customers where tier is null count",
        "T27": 'get customers where tier is not null and tier != "gold" count',
        "T28": "get orders select avg(discount)",
        "T29": "get orders select avg(coalesce(discount, 0)) as average",
        "T30": 'get customers where meta.source = "ads" count',
        "T31": 'insert customers {name: "Zeynep Kaya", email: "zeynep.kaya@example.com", country: "TR", '
               'city: "Izmir", signup: "2026-10-01T09:00:00Z"}',
        "T32": 'insert products [{sku: "SKU-2001", name: "Cable Tray", category: "office", price: 2450, stock: 30}, '
               '{sku: "SKU-2002", name: "Monitor Arm", category: "office", price: 8900, stock: 12}]',
        "T33": 'set products {stock: stock + 10} where category = "garden"',
        "T34": 'set orders {status: "cancelled"} where status = "pending" and placed < "2026-01-01"',
        "T35": 'del orders where status = "cancelled" and total < 5000',
        "T36": upsert_ref.format(sku="SKU-1003"),
        "T37": upsert_ref.format(sku="SKU-1099"),
        "T38": 'set orders {status: "paid"} where status = "pending" order placed limit 1 returning id',
        "T39": 'insert orders {customer_id: 7, status: "pending", total: 4999, placed: "2026-10-15T12:00:00Z"} '
               "returning id",
        "T40": f'del orders where status = "cancelled" and customer_id in '
               f'(get customers select id where email = "{most_email}") returning id',
    }
    for t in out:
        t["ref"] = ref[t["id"]]
    return out


if __name__ == "__main__":
    import json
    for t in tasks():
        print(json.dumps(t)[:300])
