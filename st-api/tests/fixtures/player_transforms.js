// A small player-shaped fixture: the n transform is reached through the
// player's stable URL-builder call site, not by guessing a unary function.
var _yt_player = {};
(function(g) {
  var signatureTransform = function(value) { return value.split('').reverse().join(''); };
  g.Url = class {
    constructor(value) { this.values = new Map(new URL(value).searchParams); }
    set(key, value) { this.values.set(key, value); }
    get(key) { return this.values.get(key) || null; }
    applyN() { this.set("n", this.get("n").split("").reverse().join("").toUpperCase()); }
  };
  makeUrl = function(value, signatureName, signature) {
    var url = new g.Url(value);
    url.set("alr", "yes");
    return url;
  };
})(_yt_player);
