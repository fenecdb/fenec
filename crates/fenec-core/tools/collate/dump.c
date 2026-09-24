// Prints the collation elements an ICU collation gives every assigned code
// point of planes 0 to 3 and 14, one line each: `<hex code point>
// <32-bit CE>...`, then every string it has a contraction or a prefix
// mapping for: `c <hex code point>,<hex code point>... <32-bit CE>...`.
// The collation is the locale named on the command line; an empty argument
// is the root collation.
//
// Written against macOS's libicucore, whose symbols carry no version suffix
// (cc dump.c -licucore). The declarations are ICU's own; the headers are not
// in the SDK.
#include <stdint.h>
#include <stdio.h>

typedef struct UCollator UCollator;
typedef struct UCollationElements UCollationElements;
typedef struct USet USet;
UCollator *ucol_open(const char *loc, int *status);
UCollationElements *ucol_openElements(const UCollator *, const uint16_t *text,
                                      int32_t len, int *status);
int32_t ucol_next(UCollationElements *, int *status);
void ucol_closeElements(UCollationElements *);
void ucol_getContractionsAndExpansions(const UCollator *, USet *contractions,
                                       USet *expansions, int8_t prefixes,
                                       int *status);
USet *uset_openEmpty(void);
int32_t uset_getItemCount(const USet *);
int32_t uset_getItem(const USet *, int32_t i, int32_t *start, int32_t *end,
                     uint16_t *str, int32_t cap, int *status);
int8_t u_charType(int32_t c);
void u_getVersion(uint8_t v[4]);

enum { UNASSIGNED = 0, PRIVATE_USE = 17, SURROGATE = 18 };

static int elements(const UCollator *coll, const uint16_t *s, int n) {
  int st = 0;
  UCollationElements *e = ucol_openElements(coll, s, n, &st);
  if (st > 0)
    return st;
  int32_t ce;
  while ((ce = ucol_next(e, &st)) != -1)
    printf(" %08x", (uint32_t)ce);
  printf("\n");
  ucol_closeElements(e);
  return st;
}

int main(int argc, char **argv) {
  int st = 0;
  UCollator *coll = ucol_open(argc > 1 ? argv[1] : "", &st);
  if (st > 0) {
    fprintf(stderr, "ucol_open failed: %d\n", st);
    return 1;
  }
  uint8_t v[4];
  u_getVersion(v);
  printf("icu %d.%d\n", v[0], v[1]);
  for (int32_t cp = 0; cp < 0xE1000; cp++) {
    if (cp == 0x40000)
      cp = 0xE0000;
    int t = u_charType(cp);
    // U+FFFE and U+FFFF are noncharacters, but the root gives them the
    // lowest primary and the highest. The private use area is left out by
    // its range: macOS's ICU gives Apple's characters at U+F7F0..U+F8FF
    // categories of their own, and collates them as private all the same.
    if (t == SURROGATE || t == PRIVATE_USE || (cp >= 0xE000 && cp < 0xF900) ||
        (t == UNASSIGNED && cp != 0xFFFE && cp != 0xFFFF))
      continue;
    uint16_t u[2];
    int n = 1;
    if (cp >= 0x10000) {
      u[0] = 0xD800 + ((cp - 0x10000) >> 10);
      u[1] = 0xDC00 + (cp & 0x3FF);
      n = 2;
    } else {
      u[0] = cp;
    }
    printf("%04x", cp);
    if ((st = elements(coll, u, n)) > 0) {
      fprintf(stderr, "U+%04X: %d\n", cp, st);
      return 1;
    }
  }
  USet *set = uset_openEmpty();
  ucol_getContractionsAndExpansions(coll, set, NULL, 1, &st);
  if (st > 0) {
    fprintf(stderr, "ucol_getContractionsAndExpansions failed: %d\n", st);
    return 1;
  }
  for (int32_t i = 0, count = uset_getItemCount(set); i < count; i++) {
    int32_t lo, hi;
    uint16_t s[64];
    int len = uset_getItem(set, i, &lo, &hi, s, 64, &st);
    if (st > 0 || len == 0)
      continue;
    printf("c ");
    for (int k = 0; k < len; k++) {
      const char *form = k ? ",%04x" : "%04x";
      int32_t cp = s[k];
      if (cp >= 0xD800 && cp < 0xDC00 && k + 1 < len)
        cp = 0x10000 + ((cp - 0xD800) << 10) + (s[++k] - 0xDC00);
      printf(form, cp);
    }
    if ((st = elements(coll, s, len)) > 0) {
      fprintf(stderr, "contraction %d: %d\n", i, st);
      return 1;
    }
  }
  return 0;
}
