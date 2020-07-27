CC       = rustc
CC_FLAGS = -g -C relocation-model=dynamic-no-pic
AS       = as
AS_FLAGS = -fno-pic -fno-pie
LD       = gcc -fno-pic -fno-pie -static
LD_FLAGS =
SRCS = $(wildcard *.rs) $(wildcard *.S)
BINS = $(patsubst %.S,%,$(patsubst %.rs,%,$(SRCS)))

.PHONY: all
all: $(BINS)

%: %.rs
	$(CC) $(CC_FLAGS) -o $@ $^

%: %.S
	$(AS) $(AS_FLAGS) -o $@.o $^
	$(LD) $(LD_FLAGS) -o $@ $@.o
	rm $@.o

clean:
	rm -f $(BINS)
