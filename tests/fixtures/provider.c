#include <signal.h>
#include <sys/wait.h>
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
    if (argc > 1 && strcmp(argv[1], "nested-child") == 0) {
        for (;;) pause();
    }
    printf("preview-%s\n", argc > 1 ? argv[1] : "agent");
    fflush(stdout);
    if (argc > 1 && strcmp(argv[1], "nested-parent") == 0) {
        pid_t child = 0;
        int command;
        while ((command = getchar()) != EOF) {
            if (command == 's' && child == 0) {
                child = fork();
                if (child < 0) return 1;
                if (child == 0) {
                    execl(argv[0], argv[0], "nested-child", (char *)NULL);
                    _exit(1);
                }
                puts("child-running");
                fflush(stdout);
            } else if (command == 'x' && child > 0) {
                kill(child, SIGTERM);
                waitpid(child, NULL, 0);
                child = 0;
                puts("child-stopped");
                fflush(stdout);
            }
        }
    }
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
