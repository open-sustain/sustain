Decoder fixtures: one second of a 440 Hz mono sine at 44.1 kHz in an M4A
container, one file per codec an M4A carries. Generated with:

```
ffmpeg -f lavfi -i "sine=frequency=440:duration=1:sample_rate=44100" -ac 1 \
  -c:a aac -b:a 32k -map_metadata -1 -fflags +bitexact -flags:a +bitexact sine-aac.m4a
ffmpeg -f lavfi -i "sine=frequency=440:duration=1:sample_rate=44100" -ac 1 \
  -c:a alac -map_metadata -1 -fflags +bitexact -flags:a +bitexact sine-alac.m4a
```

Presentation fixtures: a 2 ms 1 kHz burst half a second into one second of
silence, mono 44.1 kHz AAC. FFmpeg records the encoder's 1,024 priming
frames in an edit list; the second file carries no edit list, and the
priming is recorded in an iTunes `iTunSMPB` tag instead, added with
Python's `mutagen`:

```
SRC="aevalsrc='if(between(t,0.5,0.502),0.9*sin(2*PI*1000*t),0)':s=44100:d=1"
ffmpeg -f lavfi -i "$SRC" -ac 1 -c:a aac -b:a 32k -map_metadata -1 \
  -fflags +bitexact -flags:a +bitexact click-aac.m4a
ffmpeg -f lavfi -i "$SRC" -ac 1 -c:a aac -b:a 32k -map_metadata -1 \
  -fflags +bitexact -flags:a +bitexact -use_editlist 0 click-aac-itunsmpb.m4a
python3 -c "
from mutagen.mp4 import MP4, MP4FreeForm
m = MP4('click-aac-itunsmpb.m4a')
m['----:com.apple.iTunes:iTunSMPB'] = [MP4FreeForm(
    b' 00000000 00000400 00000000 000000000000AC44 00000000 00000000'
    b' 00000000 00000000 00000000 00000000 00000000 00000000')]
m.save()"
```
