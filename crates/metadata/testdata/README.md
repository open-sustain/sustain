Tag-writing fixtures: a tenth of a second of a 440 Hz mono sine, one file
per container whose tags the on-disk writer test edits and that has no
synthetic builder in the tests. Generated with:

```
SRC="sine=frequency=440:duration=0.1:sample_rate=44100"
ffmpeg -f lavfi -i "$SRC" -ac 1 -c:a pcm_s16le -map_metadata -1 \
  -fflags +bitexact -flags:a +bitexact sine.wav
ffmpeg -f lavfi -i "$SRC" -ac 1 -c:a libvorbis -q:a 0 -map_metadata -1 \
  -fflags +bitexact -flags:a +bitexact sine.ogg
ffmpeg -f lavfi -i "sine=frequency=440:duration=0.1:sample_rate=48000" -ac 1 \
  -c:a libopus -b:a 16k -map_metadata -1 -fflags +bitexact -flags:a +bitexact sine.opus
```
