/**
 * The picker's built-in Unicode emoji set — the standard Mastodon
 * category groups, as literal characters (no image assets needed).
 * Custom (uploaded) emoji are fetched separately by the picker.
 */

/** What the picker hands to its consumer on selection. */
export interface EmojiSelection {
  /** "unicode" — `value` is the literal character; "custom" — `value`
   * is the shortcode (inserted as `:value:`). */
  kind: "unicode" | "custom";
  value: string;
}

export interface UnicodeEmojiCategory {
  /** Stable id — `EmojiPicker` maps it to a translated label. */
  id: string;
  emojis: string[];
}

// Space-separated literal runs keep the tables compact; no entry
// contains a space (ZWJ sequences and VS16 variants included).
const smiles =
  "😀 😃 😄 😁 😆 😅 🤣 😂 🙂 🙃 😉 😊 😇 🥰 😍 🤩 😘 😗 😚 😙 🥲 😋 😛 😜 🤪 😝 🤑 🤗 🤭 🤫 🤔 🫡 🤐 🤨 😐 😑 😶 🫥 😏 😒 🙄 😬 🤥 🫨 😌 😔 😪 🤤 😴 😷 🤒 🤕 🤢 🤮 🤧 🥵 🥶 🥴 😵 🤯 🤠 🥳 🥸 😎 🤓 🧐";
const emotions =
  "😕 🫤 😟 🙁 ☹️ 😮 😯 😲 😳 🥺 🥹 😦 😧 😨 😰 😥 😢 😭 😱 😖 😣 😞 😓 😩 😫 🥱 😤 😡 😠 🤬 😈 👿 💀 ☠️ 💩 🤡 👹 👺 👻 👽 👾 🤖 😺 😸 😹 😻 😼 😽 🙀 😿 😾 🙈 🙉 🙊 💋 💯 💢 💥 💫 💦 💨 🕳️ 💤";
const hands =
  "👋 🤚 🖐️ ✋ 🖖 🫱 🫲 🫳 🫴 👌 🤌 🤏 ✌️ 🤞 🫰 🤟 🤘 🤙 👈 👉 👆 🖕 👇 ☝️ 🫵 👍 👎 ✊ 👊 🤛 🤜 👏 🙌 🫶 👐 🤲 🤝 🙏 ✍️ 💅 🤳 💪";
const people =
  "🦾 🦵 🦿 🦶 👂 🦻 👃 🧠 🫀 🫁 🦷 🦴 👀 👁️ 👅 👄 🫦 👶 🧒 👦 👧 🧑 👱 👨 🧔 👩 🧓 👴 👵 🙍 🙎 🙅 🙆 💁 🙋 🧏 🙇 🤦 🤷 👮 🕵️ 💂 🥷 👷 🫅 🤴 👸 👳 👲 🧕 🤵 👰 🤰 🫃 🫄 🤱 👼 🎅 🤶 🦸 🦹 🧙 🧚 🧛 🧜 🧝 🧞 🧟 🧌 💆 💇 🚶 🧍 🧎 🏃 💃 🕺 🕴️ 👯 🧖 🧗 🤺 🏇 🏌️ 🏄 🚣 🏊 ⛹️ 🏋️ 🚴 🚵 🤸 🤼 🤽 🤾 🤹 🧘 🛀 🛌 👭 👫 👬 💏 💑 👪 🗣️ 👤 👥 🫂 👣";
const animals =
  "🐵 🐒 🦍 🦧 🐶 🐕 🦮 🐩 🐺 🦊 🦝 🐱 🐈 🦁 🐯 🐅 🐆 🐴 🐎 🦄 🦓 🦌 🦬 🐮 🐂 🐃 🐄 🐷 🐖 🐗 🐽 🐏 🐑 🐐 🐪 🐫 🦙 🦒 🐘 🦣 🦏 🦛 🐭 🐁 🐀 🐹 🐰 🐇 🐿️ 🦫 🦔 🦇 🐻 🐨 🐼 🦥 🦦 🦨 🦘 🦡 🐾 🦃 🐔 🐓 🐣 🐤 🐥 🐦 🐧 🕊️ 🦅 🦆 🦢 🦉 🦤 🪶 🦩 🦚 🦜 🐸 🐊 🐢 🦎 🐍 🐲 🐉 🦕 🦖 🐳 🐋 🐬 🦭 🐟 🐠 🐡 🦈 🐙 🦑 🦐 🦞 🦀 🐚 🪸";
const nature =
  "🐌 🦋 🐛 🐜 🐝 🪲 🐞 🦗 🪳 🕷️ 🕸️ 🦂 🦟 🪰 🪱 🦠 💐 🌸 💮 🪷 🏵️ 🌹 🥀 🌺 🌻 🌼 🌷 🪻 🌱 🪴 🌲 🌳 🌴 🌵 🌾 🌿 ☘️ 🍀 🍁 🍂 🍃 🪹 🪺 🍄";
const food =
  "🍇 🍈 🍉 🍊 🍋 🍌 🍍 🥭 🍎 🍏 🍐 🍑 🍒 🍓 🫐 🥝 🍅 🫒 🥥 🥑 🍆 🥔 🥕 🌽 🌶️ 🫑 🥒 🥬 🥦 🧄 🧅 🥜 🫘 🌰 🫚 🫛 🍞 🥐 🥖 🫓 🥨 🥯 🥞 🧇 🧀 🍖 🍗 🥩 🥓 🍔 🍟 🍕 🌭 🥪 🌮 🌯 🫔 🥙 🧆 🥚 🍳 🥘 🍲 🫕 🥣 🥗 🍿 🧈 🧂 🥫 🍱 🍘 🍙 🍚 🍛 🍜 🍝 🍠 🍢 🍣 🍤 🍥 🥮 🍡 🥟 🥠 🦪 🍦 🍧 🍨 🍩 🍪 🎂 🍰 🧁 🥧 🍫 🍬 🍭 🍮 🍯 🍼 🥛 ☕ 🫖 🍵 🧃 🥤 🧋 🍶 🍺 🍻 🥂 🍷 🫗 🥃 🍸 🍹 🧉 🍾 🧊 🥄 🍴 🍽️ 🥢";
const travel =
  "🚗 🚕 🚙 🚌 🚎 🏎️ 🚓 🚑 🚒 🚐 🛻 🚚 🚛 🚜 🦯 🦽 🦼 🛴 🚲 🛵 🏍️ 🛺 🚨 🚔 🚍 🚘 🚖 🚡 🚠 🚟 🚃 🚋 🚞 🚝 🚄 🚅 🚈 🚂 🚆 🚇 🚊 🚉 ✈️ 🛫 🛬 🛩️ 💺 🛰️ 🚀 🛸 🚁 🛶 ⛵ 🚤 🛥️ 🛳️ ⛴️ 🚢 ⚓ 🪝 ⛽ 🚧 🚦 🚥 🚏 🗺️ 🗿 🗽 🗼 🏰 🏯 🏟️ 🎡 🎢 🎠 ⛲ ⛱️ 🏖️ 🏝️ 🏜️ 🌋 ⛰️ 🏔️ 🗻 🏕️ ⛺ 🛖 🏠 🏡 🏘️ 🏚️ 🏗️ 🏭 🏢 🏬 🏣 🏤 🏥 🏦 🏨 🏪 🏫 🏩 💒 🏛️ ⛪ 🕌 🕍 🛕 🕋 ⛩️ 🛤️ 🛣️ 🗾 🎑 🏞️ 🌅 🌄 🌠 🌇 🌆 🏙️ 🌃 🌌 🌉 🌁";
const activities =
  "⚽ 🏀 🏈 ⚾ 🥎 🎾 🏐 🏉 🥏 🎱 🪀 🏓 🏸 🏒 🏑 🥍 🏏 🪃 🥅 ⛳ 🪁 🏹 🎣 🤿 🥊 🥋 🎽 🛹 🛼 🛷 ⛸️ 🥌 🎿 ⛷️ 🏂 🪂 🎪 🎭 🩰 🎨 🎬 🎤 🎧 🎼 🎹 🥁 🪘 🎷 🎺 🪗 🎸 🪕 🎻 🎲 ♟️ 🎯 🎳 🎮 🎰 🧩 🧸 🪅 🪩 🪆 🃏 🀄 🎴 🎗️ 🎟️ 🎫 🏅 🥇 🥈 🥉 🏆 🎖️ 🎄 🎃 🎍 🎋 🎆 🎇 🧨 🎈 🎉 🎊 🎎 🎏 🎐 🧧 🎀 🎁 🪬 🧿";
const objects =
  "⌚ 📱 📲 💻 ⌨️ 🖥️ 🖨️ 🖱️ 🖲️ 🕹️ 🗜️ 💽 💾 💿 📀 📼 📷 📸 📹 🎥 📽️ 🎞️ 📞 ☎️ 📟 📠 📺 📻 🎙️ 🎚️ 🎛️ 🧭 ⏱️ ⏲️ ⏰ 🕰️ ⌛ ⏳ 📡 🔋 🪫 🔌 💡 🔦 🕯️ 🪔 🧯 🛢️ 💸 💵 💴 💶 💷 🪙 💰 💳 💎 ⚖️ 🪜 🧰 🪛 🔧 🔨 ⚒️ 🛠️ ⛏️ 🪚 🔩 ⚙️ 🪤 🧱 ⛓️ 🧲 🔫 💣 🪓 🔪 🗡️ ⚔️ 🛡️ 🚬 ⚰️ 🪦 ⚱️ 🏺 🔮 📿 💈 ⚗️ 🔭 🔬 🩹 🩺 💊 💉 🩸 🧬 🧫 🧪 🌡️ 🧹 🪠 🧺 🧻 🚽 🚰 🚿 🛁 🧼 🪥 🪒 🧽 🪣 🧴 🛎️ 🔑 🗝️ 🚪 🪑 🛋️ 🛏️ 🖼️ 🪞 🪟 🛍️ 🛒 🏮 🪄 ✉️ 📩 📨 📧 💌 📥 📤 📦 🏷️ 🪧 📪 📫 📬 📭 📮 📯 📜 📃 📄 📑 🧾 📊 📈 📉 🗒️ 🗓️ 📆 📅 🗑️ 📇 🗃️ 🗳️ 🗄️ 📋 📁 📂 🗂️ 🗞️ 📰 📓 📔 📒 📕 📗 📘 📙 📚 📖 🔖 🧷 🔗 📎 🖇️ 📐 📏 🧮 📌 📍 ✂️ 🖊️ 🖋️ ✒️ 🖌️ 🖍️ 📝 ✏️ 🔍 🔎 🔏 🔐 🔒 🔓";
const hearts =
  "❤️ 🩷 🧡 💛 💚 💙 🩵 💜 🖤 🩶 🤍 🤎 💔 ❤️‍🔥 ❤️‍🩹 ❣️ 💕 💞 💓 💗 💖 💘 💝 💟";
const symbols =
  "☮️ ✝️ ☪️ 🕉️ ☸️ ✡️ 🔯 🕎 ☯️ ☦️ 🛐 ⛎ ♈ ♉ ♊ ♋ ♌ ♍ ♎ ♏ ♐ ♑ ♒ ♓ 🆔 ⚛️ 🉑 ☢️ ☣️ 📴 📳 🈶 🈚 🈸 🈺 🈷️ ✴️ 🆚 🉐 ㊙️ ㊗️ 🈴 🈵 🈹 🈲 🅰️ 🅱️ 🆎 🆑 🅾️ 🆘 ❌ ⭕ 🛑 ⛔ 📛 🚫 ♨️ 🚷 🚯 🚳 🚱 🔞 📵 🚭 ❗ ❕ ❓ ❔ ‼️ ⁉️ 🔅 🔆 〽️ ⚠️ 🚸 🔱 ⚜️ 🔰 ♻️ ✅ 🈯 💹 ❇️ ✳️ ❎ 🌐 💠 Ⓜ️ 🌀 🏧 🚾 ♿ 🅿️ 🛗 🈳 🈂️ 🛂 🛃 🛄 🛅 🚹 🚺 🚼 ⚧ 🚻 🚮 🎦 📶 🈁 🔣 ℹ️ 🔤 🔡 🔠 🆖 🆗 🆙 🆒 🆕 🆓 0️⃣ 1️⃣ 2️⃣ 3️⃣ 4️⃣ 5️⃣ 6️⃣ 7️⃣ 8️⃣ 9️⃣ 🔟 🔢 #️⃣ *️⃣ ⏏️ ▶️ ⏸️ ⏯️ ⏹️ ⏺️ ⏭️ ⏮️ ⏩ ⏪ ⏫ ⏬ ◀️ 🔼 🔽 ➡️ ⬅️ ⬆️ ⬇️ ↗️ ↘️ ↙️ ↖️ ↕️ ↔️ ↪️ ↩️ ⤴️ ⤵️ 🔀 🔁 🔂 🔄 🔃 🎵 🎶 ➕ ➖ ➗ ✖️ 🟰 ♾️ 💲 💱 ™️ ©️ ®️ 〰️ ➰ ➿ 🔚 🔙 🔛 🔝 🔜 ✔️ ☑️ 🔘 🔴 🟠 🟡 🟢 🔵 🟣 ⚫ ⚪ 🟤 🔺 🔻 🔸 🔹 🔶 🔷 🔳 🔲 ▪️ ▫️ ◾ ◽ ◼️ ◻️ 🟥 🟧 🟨 🟩 🟦 🟪 ⬛ ⬜ 🟫 🔈 🔇 🔉 🔊 🔔 🔕 📣 📢 💬 💭 🗯️ ♠️ ♣️ ♥️ ♦️";
const clocks =
  "🕐 🕑 🕒 🕓 🕔 🕕 🕖 🕗 🕘 🕙 🕚 🕛 🕜 🕝 🕞 🕟 🕠 🕡 🕢 🕣 🕤 🕥 🕦 🕧";

const split = (run: string) => run.split(" ");

/** ISO 3166-1 alpha-2 codes plus the CLDR additions that carry a
 * flag (eu, un, xk, ac, cp, dg, ea, ic, ta) — each pair of regional
 * indicator symbols renders as a flag on platforms that ship them. */
const FLAG_CODES =
  "ac ad ae af ag ai al am ao aq ar as at au aw ax az ba bb bd be bf bg bh bi bj bl bm bn bo bq br bs bt bv bw by bz ca cc cd cf cg ch ci ck cl cm cn co cp cr cu cv cw cx cy cz de dg dj dk dm do dz ea ec ee eg eh er es et eu fi fj fk fm fo fr ga gb gd ge gf gg gh gi gl gm gn gp gq gr gs gt gu gw gy hk hm hn hr ht hu ic id ie il im in io iq ir is it je jm jo jp ke kg kh ki km kn kp kr kw ky kz la lb lc li lk lr ls lt lu lv ly ma mc md me mf mg mh mk ml mm mn mo mp mq mr ms mt mu mv mw mx my mz na nc ne nf ng ni nl no np nr nu nz om pa pe pf pg ph pk pl pm pn pr ps pt pw py qa re ro rs ru rw sa sb sc sd se sg sh si sj sk sl sm sn so sr ss st sv sx sy sz ta tc td tf tg th tj tk tl tm tn to tr tt tv tw tz ua ug um un us uy uz va vc ve vg vi vn vu wf ws xk ye yt za zm zw";

const flags = FLAG_CODES.split(" ").map((code) =>
  String.fromCodePoint(...[...code].map((c) => 0x1f1e6 + c.charCodeAt(0) - 97)),
);

/** The Unicode emoji catalogue, grouped like Mastodon's picker. */
export const UNICODE_EMOJIS: UnicodeEmojiCategory[] = [
  { id: "smileys", emojis: [...split(smiles), ...split(emotions)] },
  { id: "people", emojis: [...split(hands), ...split(people)] },
  { id: "animals", emojis: [...split(animals), ...split(nature)] },
  { id: "food", emojis: split(food) },
  { id: "travel", emojis: split(travel) },
  { id: "activities", emojis: split(activities) },
  { id: "objects", emojis: split(objects) },
  {
    id: "symbols",
    emojis: [...split(hearts), ...split(symbols), ...split(clocks)],
  },
  { id: "flags", emojis: flags },
];

/**
 * Names for the `:shortcode` inline autocomplete — unicode emoji you
 * can *type* (`:smile:` → 😄), unlike the picker's visual catalogue.
 * Names follow the emoji-mart/Slack conventions Mastodon users know.
 * Covers the commonly typed set; the picker remains the way to reach
 * everything else.
 */
export interface NamedEmoji {
  name: string;
  emoji: string;
}

// `name=emoji` pairs — no entry contains a space or a second `=`.
const NAMED =
  "smile=😄 smiley=😃 grin=😁 laughing=😆 sweat_smile=😅 joy=😂 rofl=🤣 slight_smile=🙂 upside_down=🙃 wink=😉 blush=😊 innocent=😇 smiling_face_with_hearts=🥰 heart_eyes=😍 star_struck=🤩 kissing_heart=😘 kissing=😗 kissing_closed_eyes=😚 kissing_smiling_eyes=😙 smiling_face_with_tear=🥲 yum=😋 stuck_out_tongue=😛 stuck_out_tongue_winking_eye=😜 zany=🤪 stuck_out_tongue_closed_eyes=😝 money_mouth=🤑 hugging=🤗 hand_over_mouth=🤭 shushing=🤫 thinking=🤔 saluting=🫡 zipper_mouth=🤐 raised_eyebrow=🤨 neutral_face=😐 expressionless=😑 no_mouth=😶 dotted_line_face=🫥 smirk=😏 unamused=😒 eye_roll=🙄 grimacing=😬 lying=🤥 shaking_face=🫨 relieved=😌 pensive=😔 sleepy=😪 drooling=🤤 sleeping=😴 mask=😷 thermometer_face=🤒 head_bandage=🤕 nauseated=🤢 vomiting=🤮 sneezing=🤧 hot_face=🥵 cold_face=🥶 woozy=🥴 dizzy_face=😵 exploding_head=🤯 cowboy=🤠 partying=🥳 disguised=🥸 sunglasses=😎 nerd=🤓 monocle=🧐 confused=😕 face_with_diagonal_mouth=🫤 worried=😟 slightly_frowning=🙁 frowning=☹️ open_mouth=😮 hushed=😯 astonished=😲 flushed=😳 pleading=🥺 face_holding_back_tears=🥹 anguished=😧 fearful=😨 cold_sweat=😰 disappointed_relieved=😥 cry=😢 sob=😭 scream=😱 confounded=😖 persevere=😣 disappointed=😞 sweat=😓 weary=😩 tired_face=😫 yawning=🥱 triumph=😤 rage=😡 angry=😠 cursing=🤬 smiling_imp=😈 imp=👿 skull=💀 skull_crossbones=☠️ poop=💩 clown=🤡 ogre=👹 goblin=👺 ghost=👻 alien=👽 space_invader=👾 robot=🤖 smiley_cat=😺 smile_cat=😸 joy_cat=😹 heart_eyes_cat=😻 smirk_cat=😼 kissing_cat=😽 scream_cat=🙀 crying_cat=😿 pouting_cat=😾 see_no_evil=🙈 hear_no_evil=🙉 speak_no_evil=🙊 kiss=💋 100=💯 anger=💢 boom=💥 dizzy=💫 sweat_drops=💦 dash=💨 hole=🕳️ zzz=💤 " +
  "wave=👋 raised_back_of_hand=🤚 raised_hand_with_fingers_splayed=🖐️ hand=✋ vulcan=🖖 rightwards_hand=🫱 leftwards_hand=🫲 palm_down=🫳 palm_up=🫴 ok_hand=👌 pinched_fingers=🤌 pinching=🤏 v=✌️ crossed_fingers=🤞 fingers_crossed=🫰 metal=🤟 horns=🤘 call_me=🤙 point_left=👈 point_right=👉 point_up_2=👆 middle_finger=🖕 point_down=👇 point_up=☝️ index_pointing_at_viewer=🫵 thumbsup=👍 thumbsdown=👎 fist_raised=✊ fist=👊 fist_left=🤛 fist_right=🤜 clap=👏 raised_hands=🙌 open_hands=👐 palms_up=🤲 handshake=🤝 pray=🙏 writing=✍️ nail_care=💅 selfie=🤳 muscle=💪 mechanical_arm=🦾 leg=🦵 foot=🦶 ear=👂 nose=👃 brain=🧠 tooth=🦷 bone=🦴 eyes=👀 eye=👁️ tongue=👅 lips=👄 " +
  "baby=👶 child=🧒 boy=👦 girl=👧 adult=🧑 man=👨 woman=👩 older_adult=🧓 old_man=👴 old_woman=👵 cop=👮 detective=🕵️ guard=💂 ninja=🥷 construction_worker=👷 prince=🤴 princess=👸 bride_with_veil=👰 pregnant=🤰 angel=👼 santa=🎅 mrs_claus=🤶 superhero=🦸 supervillain=🦹 mage=🧙 fairy=🧚 vampire=🧛 merperson=🧜 elf=🧝 genie=🧞 zombie=🧟 troll=🧌 massage=💆 haircut=💇 walking=🚶 standing=🧍 kneeling=🧎 running=🏃 dancer=💃 man_dancing=🕺 levitate=🕴️ dancers=👯 sauna=🧖 climbing=🧗 horse_racing=🏇 golf=🏌️ surfer=🏄 rowboat=🚣 swimmer=🏊 weight_lifting=🏋️ bike=🚴 mountain_bike=🚵 cartwheel=🤸 wrestling=🤼 water_polo=🤽 handball=🤾 juggler=🤹 lotus_position=🧘 bath=🛀 sleeping_bed=🛌 two_women_holding_hands=👭 couple=👫 two_men_holding_hands=👬 couplekiss=💏 couple_with_heart=💑 family=👪 " +
  "monkey_face=🐵 monkey=🐒 gorilla=🦍 orangutan=🦧 dog=🐶 dog2=🐕 guide_dog=🦮 poodle=🐩 wolf=🐺 fox=🦊 raccoon=🦝 cat=🐱 cat2=🐈 lion=🦁 tiger=🐯 tiger2=🐅 leopard=🐆 horse=🐴 racehorse=🐎 unicorn=🦄 zebra=🦓 deer=🦌 bison=🦬 cow=🐮 ox=🐂 water_buffalo=🐃 cow2=🐄 pig=🐷 pig2=🐖 boar=🐗 pig_nose=🐽 ram=🐏 sheep=🐑 goat=🐐 camel=🐪 camel2=🐫 llama=🦙 giraffe=🦒 elephant=🐘 mammoth=🦣 rhino=🦏 hippo=🦛 mouse=🐭 mouse2=🐁 rat=🐀 hamster=🐹 rabbit=🐰 rabbit2=🐇 chipmunk=🐿️ beaver=🦫 hedgehog=🦔 bat=🦇 bear=🐻 koala=🐨 panda=🐼 sloth=🦥 otter=🦦 skunk=🦨 kangaroo=🦘 badger=🦡 feet=🐾 turkey=🦃 chicken=🐔 rooster=🐓 hatching_chick=🐣 baby_chick=🐤 hatched_chick=🐥 bird=🐦 penguin=🐧 dove=🕊️ eagle=🦅 duck=🦆 swan=🦢 owl=🦉 dodo=🦤 feather=🪶 flamingo=🦩 peacock=🦚 parrot=🦜 frog=🐸 crocodile=🐊 turtle=🐢 lizard=🦎 snake=🐍 dragon_face=🐲 dragon=🐉 sauropod=🦕 t_rex=🦖 whale=🐳 whale2=🐋 dolphin=🐬 seal=🦭 fish=🐟 tropical_fish=🐠 blowfish=🐡 shark=🦈 octopus=🐙 squid=🦑 shrimp=🦐 lobster=🦞 crab=🦀 shell=🐚 coral=🪸 snail=🐌 butterfly=🦋 bug=🐛 ant=🐜 bee=🐝 beetle=🪲 ladybug=🐞 cricket=🦗 cockroach=🪳 spider=🕷️ spider_web=🕸️ scorpion=🦂 mosquito=🦟 fly=🪰 worm=🪱 microbe=🦠 bouquet=💐 cherry_blossom=🌸 white_flower=💮 lotus=🪷 rosette=🏵️ rose=🌹 wilted=🥀 hibiscus=🌺 sunflower=🌻 blossom=🌼 tulip=🌷 hyacinth=🪻 seedling=🌱 potted_plant=🪴 evergreen_tree=🌲 deciduous_tree=🌳 palm_tree=🌴 cactus=🌵 ear_of_rice=🌾 herb=🌿 shamrock=☘️ four_leaf_clover=🍀 maple_leaf=🍁 fallen_leaf=🍂 leaves=🍃 mushroom=🍄 " +
  "grapes=🍇 melon=🍈 watermelon=🍉 tangerine=🍊 lemon=🍋 banana=🍌 pineapple=🍍 mango=🥭 apple=🍎 green_apple=🍏 pear=🍐 peach=🍑 cherries=🍒 strawberry=🍓 blueberries=🫐 kiwi=🥝 tomato=🍅 olive=🫒 coconut=🥥 avocado=🥑 eggplant=🍆 potato=🥔 carrot=🥕 corn=🌽 hot_pepper=🌶️ bell_pepper=🫑 cucumber=🥒 leafy_green=🥬 broccoli=🥦 garlic=🧄 onion=🧅 peanuts=🥜 beans=🫘 chestnut=🌰 bread=🍞 croissant=🥐 baguette=🥖 pretzel=🥨 bagel=🥯 pancakes=🥞 waffle=🧇 cheese=🧀 meat_on_bone=🍖 poultry_leg=🍗 cut_of_meat=🥩 bacon=🥓 hamburger=🍔 fries=🍟 pizza=🍕 hotdog=🌭 sandwich=🥪 taco=🌮 burrito=🌯 stuffed_flatbread=🥙 falafel=🧆 egg=🥚 fried_egg=🍳 stew=🍲 bowl_with_spoon=🥣 green_salad=🥗 popcorn=🍿 butter=🧈 salt=🧂 canned_food=🥫 bento=🍱 rice_cracker=🍘 rice_ball=🍙 rice=🍚 curry=🍛 ramen=🍜 spaghetti=🍝 sweet_potato=🍠 oden=🍢 sushi=🍣 fried_shrimp=🍤 fish_cake=🍥 moon_cake=🥮 dango=🍡 dumpling=🥟 fortune_cookie=🥠 takeout_box=🥡 oyster=🦪 icecream=🍦 shaved_ice=🍧 ice_cream=🍨 doughnut=🍩 cookie=🍪 birthday=🎂 cake=🍰 cupcake=🧁 pie=🥧 chocolate=🍫 candy=🍬 lollipop=🍭 custard=🍮 honey_pot=🍯 baby_bottle=🍼 milk=🥛 coffee=☕ teapot=🫖 tea=🍵 juice_box=🧃 cup_with_straw=🥤 bubble_tea=🧋 sake=🍶 beer=🍺 beers=🍻 champagne_glass=🥂 wine=🍷 tumbler=🥃 cocktail=🍸 tropical_drink=🍹 mate=🧉 champagne=🍾 ice=🧊 spoon=🥄 fork_and_knife=🍴 plate_with_cutlery=🍽️ chopsticks=🥢 " +
  "car=🚗 taxi=🚕 blue_car=🚙 bus=🚌 trolleybus=🚎 race_car=🏎️ police_car=🚓 ambulance=🚑 fire_engine=🚒 minibus=🚐 truck=🚚 articulated_lorry=🚛 tractor=🚜 kick_scooter=🛴 bike2=🚲 motor_scooter=🛵 motorcycle=🏍️ auto_rickshaw=🛺 rotating_light=🚨 oncoming_police_car=🚔 oncoming_bus=🚍 oncoming_automobile=🚘 oncoming_taxi=🚖 aerial_tramway=🚡 suspension_railway=🚟 railway_car=🚃 tram=🚋 mountain_railway=🚞 monorail=🚝 bullettrain_side=🚄 bullettrain_front=🚅 light_rail=🚈 steam_locomotive=🚂 train2=🚆 metro=🚇 tram_car=🚊 station=🚉 airplane=✈️ departure=🛫 arrival=🛬 small_airplane=🛩️ seat=💺 satellite=🛰️ rocket=🚀 flying_saucer=🛸 helicopter=🚁 canoe=🛶 sailboat=⛵ motorboat=🛥️ yacht=🛳️ ferry=⛴️ ship=🚢 anchor=⚓ fuel_pump=⛽ construction=🚧 vertical_traffic_light=🚦 traffic_light=🚥 bus_stop=🚏 map=🗺️ moyai=🗿 statue_of_liberty=🗽 tokyo_tower=🗼 castle=🏰 japanese_castle=🏯 stadium=🏟️ ferris_wheel=🎡 roller_coaster=🎢 carousel=🎠 fountain=⛲ umbrella_on_ground=⛱️ beach=🏖️ island=🏝️ desert=🏜️ volcano=🌋 mountain=⛰️ snow_capped_mountain=🏔️ mount_fuji=🗻 camping=🏕️ tent=⛺ hut=🛖 house=🏠 house_with_garden=🏡 homes=🏘️ derelict_house=🏚️ construction_site=🏗️ factory=🏭 office=🏢 department_store=🏬 post_office=🏣 european_post_office=🏤 hospital=🏥 bank=🏦 hotel=🏨 convenience_store=🏪 school=🏫 love_hotel=🏩 wedding=💒 classical_building=🏛️ church=⛪ mosque=🕌 synagogue=🕍 hindu_temple=🛕 kaaba=🕋 shinto_shrine=⛩️ sunrise=🌅 sunrise_over_mountains=🌄 stars=🌠 city_dusk=🌇 city_sunset=🌆 cityscape=🏙️ night_with_stars=🌃 milky_way=🌌 bridge_at_night=🌉 foggy=🌁 " +
  "soccer=⚽ basketball=🏀 football=🏈 baseball=⚾ softball=🥎 tennis=🎾 volleyball=🏐 rugby=🏉 flying_disc=🥏 8ball=🎱 ping_pong=🏓 badminton=🏸 hockey=🏒 field_hockey=🏑 lacrosse=🥍 cricket_game=🏏 boomerang=🪃 goal=🥅 golf_flag=⛳ kite=🪁 bow_and_arrow=🏹 fishing=🎣 diving_mask=🤿 boxing_glove=🥊 martial_arts=🥋 running_shirt=🎽 skateboard=🛹 roller_skate=🛼 sled=🛷 ice_skate=⛸️ curling=🥌 ski=🎿 skier=⛷️ snowboarder=🏂 parachute=🪂 circus_tent=🎪 performing_arts=🎭 ballet=🩰 art=🎨 clapper=🎬 microphone=🎤 headphones=🎧 musical_score=🎼 musical_keyboard=🎹 drum=🥁 long_drum=🪘 saxophone=🎷 trumpet=🎺 accordion=🪗 guitar=🎸 banjo=🪕 violin=🎻 game_die=🎲 chess=♟️ dart=🎯 bowling=🎳 video_game=🎮 slot_machine=🎰 puzzle=🧩 teddy=🧸 pinata=🪅 mirror_ball=🪩 nesting_dolls=🪆 flower_playing_cards=🎴 tickets=🎟️ ticket=🎫 medal=🏅 trophy=🏆 christmas_tree=🎄 jack_o_lantern=🎃 tanabata_tree=🎋 fireworks=🎆 sparkler=🎇 firecracker=🧨 balloon=🎈 tada=🎉 confetti=🎊 dolls=🎎 carp_streamer=🎏 wind_chime=🎐 red_envelope=🧧 ribbon=🎀 gift=🎁 magic_wand=🪄 hamsa=🪬 nazar=🧿 " +
  "watch=⌚ iphone=📱 calling=📲 computer=💻 keyboard=⌨️ desktop=🖥️ printer=🖨️ mouse_three_button=🖱️ trackball=🖲️ joystick=🕹️ minidisc=💽 floppy_disk=💾 cd=💿 dvd=📀 vhs=📼 camera=📷 camera_flash=📸 video_camera=📹 movie_camera=🎥 projector=📽️ film_strip=🎞️ telephone_receiver=📞 phone=☎️ pager=📟 fax=📠 tv=📺 radio=📻 studio_microphone=🎙️ level_slider=🎚️ control_knobs=🎛️ compass=🧭 stopwatch=⏱️ timer=⏲️ alarm_clock=⏰ mantlepiece_clock=🕰️ hourglass=⌛ hourglass_flowing=⏳ satellite_dish=📡 battery=🔋 low_battery=🪫 electric_plug=🔌 bulb=💡 flashlight=🔦 candle=🕯️ fire_extinguisher=🧯 oil_drum=🛢️ money_with_wings=💸 dollar=💵 yen=💴 euro=💶 pound=💷 coin=🪙 moneybag=💰 credit_card=💳 gem=💎 scales=⚖️ ladder=🪜 toolbox=🧰 screwdriver=🪛 wrench=🔧 hammer=🔨 hammer_and_pick=⚒️ tools=🛠️ pick=⛏️ saw=🪚 nut_and_bolt=🔩 gear=⚙️ mouse_trap=🪤 brick=🧱 chains=⛓️ magnet=🧲 gun=🔫 bomb=💣 axe=🪓 knife=🔪 dagger=🗡️ crossed_swords=⚔️ shield=🛡️ cigarette=🚬 coffin=⚰️ headstone=🪦 urn=⚱️ amphora=🏺 crystal_ball=🔮 prayer_beads=📿 barber=💈 alembic=⚗️ telescope=🔭 microscope=🔬 bandage=🩹 stethoscope=🩺 pill=💊 syringe=💉 blood=🩸 dna=🧬 petri_dish=🧫 test_tube=🧪 thermometer=🌡️ broom=🧹 plunger=🪠 basket=🧺 toilet_paper=🧻 toilet=🚽 potable_water=🚰 shower=🚿 bathtub=🛁 soap=🧼 toothbrush=🪥 razor=🪒 sponge=🧽 bucket=🪣 lotion=🧴 bellhop=🛎️ key=🔑 old_key=🗝️ door=🚪 chair=🪑 couch=🛋️ bed=🛏️ frame_with_picture=🖼️ mirror=🪞 window=🪟 shopping_bags=🛍️ shopping_cart=🛒 lantern=🏮 envelope=✉️ incoming_envelope=📨 email=📧 love_letter=💌 inbox_tray=📥 outbox_tray=📤 package=📦 label=🏷️ mailbox_closed=📪 mailbox=📫 mailbox_with_mail=📬 mailbox_with_no_mail=📭 postbox=📮 postal_horn=📯 scroll=📜 page_with_curl=📃 page_facing_up=📄 bookmark_tabs=📑 receipt=🧾 bar_chart=📊 chart_with_upwards_trend=📈 chart_with_downwards_trend=📉 spiral_notepad=🗒️ spiral_calendar=🗓️ date=📅 calendar=📆 wastebasket=🗑️ card_index=📇 card_file_box=🗃️ ballot_box=🗳️ file_cabinet=🗄️ clipboard=📋 file_folder=📁 open_file_folder=📂 card_index_dividers=🗂️ newspaper_roll=🗞️ newspaper=📰 notebook=📓 notebook_with_decorative_cover=📔 ledger=📒 closed_book=📕 green_book=📗 blue_book=📘 orange_book=📙 books=📚 book=📖 bookmark=🔖 safety_pin=🧷 link=🔗 paperclip=📎 linked_paperclips=🖇️ triangular_ruler=📐 straight_ruler=📏 abacus=🧮 pushpin=📌 round_pushpin=📍 scissors=✂️ pen_ballpoint=🖊️ pen_fountain=🖋️ black_nib=✒️ paintbrush=🖌️ crayon=🖍️ memo=📝 pencil=✏️ mag=🔍 mag_right=🔎 lock_with_pen=🔏 closed_lock_with_key=🔐 lock=🔒 unlock=🔓 " +
  "heart=❤️ pink_heart=🩷 orange_heart=🧡 yellow_heart=💛 green_heart=💚 blue_heart=💙 light_blue_heart=🩵 purple_heart=💜 black_heart=🖤 grey_heart=🩶 white_heart=🤍 brown_heart=🤎 broken_heart=💔 heart_on_fire=❤️‍🔥 mending_heart=❤️‍🩹 heart_exclamation=❣️ two_hearts=💕 revolving_hearts=💞 heartbeat=💓 heartpulse=💗 sparkling_heart=💖 cupid=💘 gift_heart=💝 heart_decoration=💟 peace=☮️ cross=✝️ star_and_crescent=☪️ om=🕉️ wheel_of_dharma=☸️ star_of_david=✡️ six_pointed_star=🔯 menorah=🕎 yin_yang=☯️ orthodox_cross=☦️ place_of_worship=🛐 aries=♈ taurus=♉ gemini=♊ cancer=♋ leo=♌ virgo=♍ libra=♎ scorpius=♏ sagittarius=♐ capricorn=♑ aquarius=♒ pisces=♓ atom=⚛️ radioactive=☢️ biohazard=☣️ vs=🆚 sos=🆘 x=❌ o=⭕ stop_sign=🛑 no_entry=⛔ name_badge=📛 no_entry_sign=🚫 hotsprings=♨️ no_pedestrians=🚷 do_not_litter=🚯 no_bicycles=🚳 underage=🔞 no_mobile_phones=📵 no_smoking=🚭 exclamation=❗ grey_exclamation=❕ question=❓ grey_question=❔ bangbang=‼️ interrobang=⁉️ low_brightness=🔅 high_brightness=🔆 warning=⚠️ children_crossing=🚸 trident=🔱 fleur_de_lis=⚜️ beginner=🔰 recycle=♻️ white_check_mark=✅ currency_exchange=💹 sparkle=❇️ eight_spoked_asterisk=✳️ negative_squared_cross_mark=❎ globe_with_meridians=🌐 diamond_shape_with_a_dot_inside=💠 m=Ⓜ️ cyclone=🌀 atm=🏧 wc=🚾 wheelchair=♿ parking=🅿️ mens=🚹 womens=🚺 baby_symbol=🚼 transgender=⚧ restroom=🚻 put_litter_in_its_place=🚮 cinema=🎦 signal_strength=📶 koko=🈁 symbols=🔣 information_source=ℹ️ abc=🔤 abcd=🔡 capital_abcd=🔠 ng=🆖 ok=🆗 up=🆙 cool=🆒 new=🆕 free=🆓 zero=0️⃣ one=1️⃣ two=2️⃣ three=3️⃣ four=4️⃣ five=5️⃣ six=6️⃣ seven=7️⃣ eight=8️⃣ nine=9️⃣ keycap_ten=🔟 arrow_forward=▶️ pause_button=⏸️ stop_button=⏹️ record_button=⏺️ track_next=⏭️ track_previous=⏮️ fast_forward=⏩ rewind=⏪ arrow_backward=◀️ arrow_up_small=🔼 arrow_down_small=🔽 arrow_right=➡️ arrow_left=⬅️ arrow_up=⬆️ arrow_down=⬇️ arrow_up_down=↕️ left_right_arrow=↔️ arrow_right_hook=↪️ leftwards_arrow_with_hook=↩️ arrow_heading_up=⤴️ arrow_heading_down=⤵️ twisted_rightwards_arrows=🔀 repeat=🔁 repeat_one=🔂 arrows_counterclockwise=🔄 arrows_clockwise=🔃 musical_note=🎵 notes=🎶 heavy_plus_sign=➕ heavy_minus_sign=➖ heavy_division_sign=➗ heavy_multiplication_x=✖️ heavy_equals_sign=🟰 infinity=♾️ dollar_sign=💲 tm=™️ copyright=©️ registered=®️ wavy_dash=〰️ curly_loop=➰ loop=➿ end=🔚 back=🔙 on=🔛 top=🔝 soon=🔜 heavy_check_mark=✔️ ballot_box_with_check=☑️ radio_button=🔘 red_circle=🔴 orange_circle=🟠 yellow_circle=🟡 green_circle=🟢 blue_circle=🔵 purple_circle=🟣 black_circle=⚫ white_circle=⚪ brown_circle=🟤 small_red_triangle=🔺 small_red_triangle_down=🔻 small_orange_diamond=🔸 small_blue_diamond=🔹 large_orange_diamond=🔶 large_blue_diamond=🔷 black_square_button=🔳 white_square_button=🔲 red_square=🟥 orange_square=🟧 yellow_square=🟨 green_square=🟩 blue_square=🟦 purple_square=🟪 black_large_square=⬛ white_large_square=⬜ brown_square=🟫 speaker=🔈 mute=🔇 sound=🔉 loud_sound=🔊 bell=🔔 no_bell=🔕 mega=📣 loudspeaker=📢 speech_balloon=💬 thought_balloon=💭 spades=♠️ clubs=♣️ hearts=♥️ diamonds=♦️ " +
  "checkered_flag=🏁 triangular_flag=🚩 crossed_flags=🎌 waving_black_flag=🏴 white_flag=🏳️ rainbow_flag=🏳️‍🌈 transgender_flag=🏳️‍⚧️ pirate_flag=🏴‍☠️";

/** Searchable `name` → unicode char map for `:` completion. Includes
 * `flag_<iso>` for every flag (Mastodon's flag naming). */
export const NAMED_UNICODE_EMOJIS: NamedEmoji[] = [
  ...NAMED.split(" ")
    .filter(Boolean)
    .map((pair) => {
      const i = pair.indexOf("=");
      return { name: pair.slice(0, i), emoji: pair.slice(i + 1) };
    }),
  ...FLAG_CODES.split(" ").map((code, i) => ({
    name: `flag_${code}`,
    emoji: flags[i],
  })),
];
