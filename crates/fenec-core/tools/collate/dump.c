// Prints the collation elements ICU's `tr` collation gives each code point
// the table covers, and each printable ASCII character followed by each
// combining mark (which is how gen.py finds the contractions), one line
// each: `<hex code points> <32-bit CE>...`.
//
// Written against macOS's libicucore, whose symbols carry no version suffix
// (cc dump.c -licucore). The declarations are ICU's own; the headers are not
// in the SDK.
#include <stdint.h>
#include <stdio.h>

typedef struct UCollator UCollator;
typedef struct UCollationElements UCollationElements;
UCollator *ucol_open(const char *loc, int *status);
UCollationElements *ucol_openElements(const UCollator *, const uint16_t *text,
                                      int32_t len, int *status);
int32_t ucol_next(UCollationElements *, int *status);
void ucol_closeElements(UCollationElements *);
void u_getVersion(uint8_t v[4]);

static UCollator *coll;

static void dump(const uint16_t *u, int n) {
  int st = 0;
  UCollationElements *e = ucol_openElements(coll, u, n, &st);
  for (int i = 0; i < n; i++)
    printf("%s%04x", i ? "+" : "", u[i]);
  int32_t ce;
  while ((ce = ucol_next(e, &st)) != -1)
    printf(" %08x", (uint32_t)ce);
  printf("\n");
  ucol_closeElements(e);
}

int main(void) {
  int st = 0;
  coll = ucol_open("tr", &st);
  if (st > 0) {
    fprintf(stderr, "ucol_open failed: %d\n", st);
    return 1;
  }
  uint8_t v[4];
  u_getVersion(v);
  printf("icu %d.%d\n", v[0], v[1]);
  uint16_t u[2];
  for (int cp = 0; cp < 0x2100; cp++) {
    u[0] = cp;
    dump(u, 1);
  }
  for (int cp = 0x2c60; cp < 0x2c80; cp++) {
    u[0] = cp;
    dump(u, 1);
  }
  u[0] = 0xfeff;
  dump(u, 1);
  for (int b = 0x20; b < 0x7f; b++)
    for (int m = 0x300; m < 0x370; m++) {
      u[0] = b;
      u[1] = m;
      dump(u, 2);
    }
  return 0;
}
