# Rename rules for the upstream sync: run as `perl -i -p scripts/rename-rules.pl FILE...`.
# Idempotent. Identifiers that belong to other projects stay as they are.
s#github\.com/storytold/soundcraft#KEEP_GHURL#g;
s#storytold/soundcraft#KEEP_REPO#g;
s#craft_update::suite::SOUNDCRAFT#KEEP_SUITE#g;
s/SoundCraft\.(app|AppDir|Session|Audio|Midi)/LiViWave.$1/g;
s/Soundcraft(?=[A-Za-z])/LiviWave/g;
s/Soundcraft/LiVi Wave/g;
s/SoundCraft/LiVi Wave/g;
s/SOUNDCRAFT/LIVIWAVE/g;
s/soundcraft/liviwave/g;
s#KEEP_GHURL#github.com/storytold/soundcraft#g;
s#KEEP_REPO#storytold/soundcraft#g;
s#KEEP_SUITE#craft_update::suite::SOUNDCRAFT#g;
