# Scope the classic provider to NH without rebuilding NH or changing its nom.
{ lib, callPackage, symlinkJoin, makeBinaryWrapper, iets, nix, nh }:
let
  classic = callPackage ./classic.nix { inherit iets nix; };
in
symlinkJoin {
  inherit (nh) pname version;
  name = "${nh.pname}-${nh.version}";
  paths = [ nh ];
  nativeBuildInputs = [ makeBinaryWrapper ];
  passthru = nh.passthru or { };
  meta = nh.meta;
  postBuild = ''
    # NH may already be wrapped: replace only our link, then execute the
    # original entry point so its wrapper and runtime dependencies stay intact.
    rm "$out/bin/nh"
    makeBinaryWrapper ${lib.getExe nh} "$out/bin/nh" \
      --prefix PATH : ${lib.makeBinPath [ classic ]}
  '';
}
