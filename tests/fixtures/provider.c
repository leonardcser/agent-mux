#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "status") == 0) {
        FILE *f = fopen(getenv("AGENT_MUX_TEST_STATUSES"), "r");
        if (!f) return 1;
        int c;
        while ((c = fgetc(f)) != EOF) putchar(c);
        fclose(f);
        return 0;
    }
    printf("preview-%s\n", argc > 1 ? argv[1] : "agent");
    fflush(stdout);
    if (argc > 2) {
        char previous[4096] = "", text[4096];
        for (;;) {
            FILE *f = fopen(argv[2], "r");
            if (f) {
                size_t n = fread(text, 1, sizeof(text) - 1, f);
                text[n] = 0;
                fclose(f);
                if (strcmp(text, previous) != 0) {
                    printf("\033[2J\033[H%s", text);
                    fflush(stdout);
                    strcpy(previous, text);
                }
            }
            usleep(5000);
        }
    }
    for (;;) pause();
}
