// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;

/**
 * gci i38/rf ({@code gcg-g4oo-oome-ladder-review-findings} item 5): the
 * fail-first twin of {@code GciOo22ReflectLinkOomeProbe} with constants too
 * large for the room the fill leaves.
 *
 * <p>CratonVM builds a class's {@code ConstantValue} Strings when it links the
 * class (preparation); reflection links the class it lists
 * ({@code getDeclaredFields0}), and before gci i22/oo that allocation never
 * collected, so on a heap full of garbage the listing threw
 * {@code OutOfMemoryError} where HotSpot (which builds the Strings with the
 * mirror at class load) answers. The i22/oo fix collects once and links again
 * ({@code CRATONVM_GC_CLASS_PREP_RECLAIM=0} restores the old door). The older
 * probe's eight constants (about 480 KB) always fit: vi26-oo2 RD measured
 * {@code Runtime.freeMemory()} of 8.5-10.7 MB between the drop and the listing
 * on the kill rows, so its control never refused. Here each class has 256
 * constants of 60 004 characters (about 15.4 MB of Latin-1 bytes), more than
 * that room.
 *
 * <p>Each stage loads (does not link or initialize) one nested class, fills
 * the heap with LIVE chunks (128 KiB, 8 KiB, 2 KiB rungs, each until its first
 * error), drops them by returning, and lists the class's fields. stderr:
 * {@code stage <i> fill-begin}, then {@code chunks=} and {@code free-at-call=}
 * ({@code Runtime.freeMemory()} between the drop and the listing with the
 * argument {@code diag}, else {@code -1}). stdout is the same in both modes.
 *
 * <p>stdout (HotSpot 25, any collector, {@code -Xmx64m}):
 * <pre>
 *   stage 0 fill=oome getDeclaredFields=ok fields=256 sum=15361024
 *   stage 1 fill=oome getDeclaredFields=ok fields=256 sum=15361024
 *   PROBE-END
 * </pre>
 * {@code getDeclaredFields=oome fields=-1 sum=-1} is the pre-i22 CratonVM
 * answer. {@code sum} reads every constant back (it initializes the class), so
 * a wrong String after the retry shows.
 *
 * <p>{@code java -XX:+UseSerialGC -Xmx64m GciRf38WideReflectLinkOomeProbe}
 */
public final class GciRf38WideReflectLinkOomeProbe {
    static final String C10 = "0123456789";
    static final String C100 = C10 + C10 + C10 + C10 + C10 + C10 + C10 + C10 + C10 + C10;
    static final String C1K =
            C100 + C100 + C100 + C100 + C100 + C100 + C100 + C100 + C100 + C100;
    static final String C10K = C1K + C1K + C1K + C1K + C1K + C1K + C1K + C1K + C1K + C1K;
    static final String C60K = C10K + C10K + C10K + C10K + C10K + C10K;

    static final class Wide0 {
        static final String F0 = C60K + "a000";
        static final String F1 = C60K + "a001";
        static final String F2 = C60K + "a002";
        static final String F3 = C60K + "a003";
        static final String F4 = C60K + "a004";
        static final String F5 = C60K + "a005";
        static final String F6 = C60K + "a006";
        static final String F7 = C60K + "a007";
        static final String F8 = C60K + "a008";
        static final String F9 = C60K + "a009";
        static final String F10 = C60K + "a010";
        static final String F11 = C60K + "a011";
        static final String F12 = C60K + "a012";
        static final String F13 = C60K + "a013";
        static final String F14 = C60K + "a014";
        static final String F15 = C60K + "a015";
        static final String F16 = C60K + "a016";
        static final String F17 = C60K + "a017";
        static final String F18 = C60K + "a018";
        static final String F19 = C60K + "a019";
        static final String F20 = C60K + "a020";
        static final String F21 = C60K + "a021";
        static final String F22 = C60K + "a022";
        static final String F23 = C60K + "a023";
        static final String F24 = C60K + "a024";
        static final String F25 = C60K + "a025";
        static final String F26 = C60K + "a026";
        static final String F27 = C60K + "a027";
        static final String F28 = C60K + "a028";
        static final String F29 = C60K + "a029";
        static final String F30 = C60K + "a030";
        static final String F31 = C60K + "a031";
        static final String F32 = C60K + "a032";
        static final String F33 = C60K + "a033";
        static final String F34 = C60K + "a034";
        static final String F35 = C60K + "a035";
        static final String F36 = C60K + "a036";
        static final String F37 = C60K + "a037";
        static final String F38 = C60K + "a038";
        static final String F39 = C60K + "a039";
        static final String F40 = C60K + "a040";
        static final String F41 = C60K + "a041";
        static final String F42 = C60K + "a042";
        static final String F43 = C60K + "a043";
        static final String F44 = C60K + "a044";
        static final String F45 = C60K + "a045";
        static final String F46 = C60K + "a046";
        static final String F47 = C60K + "a047";
        static final String F48 = C60K + "a048";
        static final String F49 = C60K + "a049";
        static final String F50 = C60K + "a050";
        static final String F51 = C60K + "a051";
        static final String F52 = C60K + "a052";
        static final String F53 = C60K + "a053";
        static final String F54 = C60K + "a054";
        static final String F55 = C60K + "a055";
        static final String F56 = C60K + "a056";
        static final String F57 = C60K + "a057";
        static final String F58 = C60K + "a058";
        static final String F59 = C60K + "a059";
        static final String F60 = C60K + "a060";
        static final String F61 = C60K + "a061";
        static final String F62 = C60K + "a062";
        static final String F63 = C60K + "a063";
        static final String F64 = C60K + "a064";
        static final String F65 = C60K + "a065";
        static final String F66 = C60K + "a066";
        static final String F67 = C60K + "a067";
        static final String F68 = C60K + "a068";
        static final String F69 = C60K + "a069";
        static final String F70 = C60K + "a070";
        static final String F71 = C60K + "a071";
        static final String F72 = C60K + "a072";
        static final String F73 = C60K + "a073";
        static final String F74 = C60K + "a074";
        static final String F75 = C60K + "a075";
        static final String F76 = C60K + "a076";
        static final String F77 = C60K + "a077";
        static final String F78 = C60K + "a078";
        static final String F79 = C60K + "a079";
        static final String F80 = C60K + "a080";
        static final String F81 = C60K + "a081";
        static final String F82 = C60K + "a082";
        static final String F83 = C60K + "a083";
        static final String F84 = C60K + "a084";
        static final String F85 = C60K + "a085";
        static final String F86 = C60K + "a086";
        static final String F87 = C60K + "a087";
        static final String F88 = C60K + "a088";
        static final String F89 = C60K + "a089";
        static final String F90 = C60K + "a090";
        static final String F91 = C60K + "a091";
        static final String F92 = C60K + "a092";
        static final String F93 = C60K + "a093";
        static final String F94 = C60K + "a094";
        static final String F95 = C60K + "a095";
        static final String F96 = C60K + "a096";
        static final String F97 = C60K + "a097";
        static final String F98 = C60K + "a098";
        static final String F99 = C60K + "a099";
        static final String F100 = C60K + "a100";
        static final String F101 = C60K + "a101";
        static final String F102 = C60K + "a102";
        static final String F103 = C60K + "a103";
        static final String F104 = C60K + "a104";
        static final String F105 = C60K + "a105";
        static final String F106 = C60K + "a106";
        static final String F107 = C60K + "a107";
        static final String F108 = C60K + "a108";
        static final String F109 = C60K + "a109";
        static final String F110 = C60K + "a110";
        static final String F111 = C60K + "a111";
        static final String F112 = C60K + "a112";
        static final String F113 = C60K + "a113";
        static final String F114 = C60K + "a114";
        static final String F115 = C60K + "a115";
        static final String F116 = C60K + "a116";
        static final String F117 = C60K + "a117";
        static final String F118 = C60K + "a118";
        static final String F119 = C60K + "a119";
        static final String F120 = C60K + "a120";
        static final String F121 = C60K + "a121";
        static final String F122 = C60K + "a122";
        static final String F123 = C60K + "a123";
        static final String F124 = C60K + "a124";
        static final String F125 = C60K + "a125";
        static final String F126 = C60K + "a126";
        static final String F127 = C60K + "a127";
        static final String F128 = C60K + "a128";
        static final String F129 = C60K + "a129";
        static final String F130 = C60K + "a130";
        static final String F131 = C60K + "a131";
        static final String F132 = C60K + "a132";
        static final String F133 = C60K + "a133";
        static final String F134 = C60K + "a134";
        static final String F135 = C60K + "a135";
        static final String F136 = C60K + "a136";
        static final String F137 = C60K + "a137";
        static final String F138 = C60K + "a138";
        static final String F139 = C60K + "a139";
        static final String F140 = C60K + "a140";
        static final String F141 = C60K + "a141";
        static final String F142 = C60K + "a142";
        static final String F143 = C60K + "a143";
        static final String F144 = C60K + "a144";
        static final String F145 = C60K + "a145";
        static final String F146 = C60K + "a146";
        static final String F147 = C60K + "a147";
        static final String F148 = C60K + "a148";
        static final String F149 = C60K + "a149";
        static final String F150 = C60K + "a150";
        static final String F151 = C60K + "a151";
        static final String F152 = C60K + "a152";
        static final String F153 = C60K + "a153";
        static final String F154 = C60K + "a154";
        static final String F155 = C60K + "a155";
        static final String F156 = C60K + "a156";
        static final String F157 = C60K + "a157";
        static final String F158 = C60K + "a158";
        static final String F159 = C60K + "a159";
        static final String F160 = C60K + "a160";
        static final String F161 = C60K + "a161";
        static final String F162 = C60K + "a162";
        static final String F163 = C60K + "a163";
        static final String F164 = C60K + "a164";
        static final String F165 = C60K + "a165";
        static final String F166 = C60K + "a166";
        static final String F167 = C60K + "a167";
        static final String F168 = C60K + "a168";
        static final String F169 = C60K + "a169";
        static final String F170 = C60K + "a170";
        static final String F171 = C60K + "a171";
        static final String F172 = C60K + "a172";
        static final String F173 = C60K + "a173";
        static final String F174 = C60K + "a174";
        static final String F175 = C60K + "a175";
        static final String F176 = C60K + "a176";
        static final String F177 = C60K + "a177";
        static final String F178 = C60K + "a178";
        static final String F179 = C60K + "a179";
        static final String F180 = C60K + "a180";
        static final String F181 = C60K + "a181";
        static final String F182 = C60K + "a182";
        static final String F183 = C60K + "a183";
        static final String F184 = C60K + "a184";
        static final String F185 = C60K + "a185";
        static final String F186 = C60K + "a186";
        static final String F187 = C60K + "a187";
        static final String F188 = C60K + "a188";
        static final String F189 = C60K + "a189";
        static final String F190 = C60K + "a190";
        static final String F191 = C60K + "a191";
        static final String F192 = C60K + "a192";
        static final String F193 = C60K + "a193";
        static final String F194 = C60K + "a194";
        static final String F195 = C60K + "a195";
        static final String F196 = C60K + "a196";
        static final String F197 = C60K + "a197";
        static final String F198 = C60K + "a198";
        static final String F199 = C60K + "a199";
        static final String F200 = C60K + "a200";
        static final String F201 = C60K + "a201";
        static final String F202 = C60K + "a202";
        static final String F203 = C60K + "a203";
        static final String F204 = C60K + "a204";
        static final String F205 = C60K + "a205";
        static final String F206 = C60K + "a206";
        static final String F207 = C60K + "a207";
        static final String F208 = C60K + "a208";
        static final String F209 = C60K + "a209";
        static final String F210 = C60K + "a210";
        static final String F211 = C60K + "a211";
        static final String F212 = C60K + "a212";
        static final String F213 = C60K + "a213";
        static final String F214 = C60K + "a214";
        static final String F215 = C60K + "a215";
        static final String F216 = C60K + "a216";
        static final String F217 = C60K + "a217";
        static final String F218 = C60K + "a218";
        static final String F219 = C60K + "a219";
        static final String F220 = C60K + "a220";
        static final String F221 = C60K + "a221";
        static final String F222 = C60K + "a222";
        static final String F223 = C60K + "a223";
        static final String F224 = C60K + "a224";
        static final String F225 = C60K + "a225";
        static final String F226 = C60K + "a226";
        static final String F227 = C60K + "a227";
        static final String F228 = C60K + "a228";
        static final String F229 = C60K + "a229";
        static final String F230 = C60K + "a230";
        static final String F231 = C60K + "a231";
        static final String F232 = C60K + "a232";
        static final String F233 = C60K + "a233";
        static final String F234 = C60K + "a234";
        static final String F235 = C60K + "a235";
        static final String F236 = C60K + "a236";
        static final String F237 = C60K + "a237";
        static final String F238 = C60K + "a238";
        static final String F239 = C60K + "a239";
        static final String F240 = C60K + "a240";
        static final String F241 = C60K + "a241";
        static final String F242 = C60K + "a242";
        static final String F243 = C60K + "a243";
        static final String F244 = C60K + "a244";
        static final String F245 = C60K + "a245";
        static final String F246 = C60K + "a246";
        static final String F247 = C60K + "a247";
        static final String F248 = C60K + "a248";
        static final String F249 = C60K + "a249";
        static final String F250 = C60K + "a250";
        static final String F251 = C60K + "a251";
        static final String F252 = C60K + "a252";
        static final String F253 = C60K + "a253";
        static final String F254 = C60K + "a254";
        static final String F255 = C60K + "a255";
    }

    static final class Wide1 {
        static final String F0 = C60K + "b000";
        static final String F1 = C60K + "b001";
        static final String F2 = C60K + "b002";
        static final String F3 = C60K + "b003";
        static final String F4 = C60K + "b004";
        static final String F5 = C60K + "b005";
        static final String F6 = C60K + "b006";
        static final String F7 = C60K + "b007";
        static final String F8 = C60K + "b008";
        static final String F9 = C60K + "b009";
        static final String F10 = C60K + "b010";
        static final String F11 = C60K + "b011";
        static final String F12 = C60K + "b012";
        static final String F13 = C60K + "b013";
        static final String F14 = C60K + "b014";
        static final String F15 = C60K + "b015";
        static final String F16 = C60K + "b016";
        static final String F17 = C60K + "b017";
        static final String F18 = C60K + "b018";
        static final String F19 = C60K + "b019";
        static final String F20 = C60K + "b020";
        static final String F21 = C60K + "b021";
        static final String F22 = C60K + "b022";
        static final String F23 = C60K + "b023";
        static final String F24 = C60K + "b024";
        static final String F25 = C60K + "b025";
        static final String F26 = C60K + "b026";
        static final String F27 = C60K + "b027";
        static final String F28 = C60K + "b028";
        static final String F29 = C60K + "b029";
        static final String F30 = C60K + "b030";
        static final String F31 = C60K + "b031";
        static final String F32 = C60K + "b032";
        static final String F33 = C60K + "b033";
        static final String F34 = C60K + "b034";
        static final String F35 = C60K + "b035";
        static final String F36 = C60K + "b036";
        static final String F37 = C60K + "b037";
        static final String F38 = C60K + "b038";
        static final String F39 = C60K + "b039";
        static final String F40 = C60K + "b040";
        static final String F41 = C60K + "b041";
        static final String F42 = C60K + "b042";
        static final String F43 = C60K + "b043";
        static final String F44 = C60K + "b044";
        static final String F45 = C60K + "b045";
        static final String F46 = C60K + "b046";
        static final String F47 = C60K + "b047";
        static final String F48 = C60K + "b048";
        static final String F49 = C60K + "b049";
        static final String F50 = C60K + "b050";
        static final String F51 = C60K + "b051";
        static final String F52 = C60K + "b052";
        static final String F53 = C60K + "b053";
        static final String F54 = C60K + "b054";
        static final String F55 = C60K + "b055";
        static final String F56 = C60K + "b056";
        static final String F57 = C60K + "b057";
        static final String F58 = C60K + "b058";
        static final String F59 = C60K + "b059";
        static final String F60 = C60K + "b060";
        static final String F61 = C60K + "b061";
        static final String F62 = C60K + "b062";
        static final String F63 = C60K + "b063";
        static final String F64 = C60K + "b064";
        static final String F65 = C60K + "b065";
        static final String F66 = C60K + "b066";
        static final String F67 = C60K + "b067";
        static final String F68 = C60K + "b068";
        static final String F69 = C60K + "b069";
        static final String F70 = C60K + "b070";
        static final String F71 = C60K + "b071";
        static final String F72 = C60K + "b072";
        static final String F73 = C60K + "b073";
        static final String F74 = C60K + "b074";
        static final String F75 = C60K + "b075";
        static final String F76 = C60K + "b076";
        static final String F77 = C60K + "b077";
        static final String F78 = C60K + "b078";
        static final String F79 = C60K + "b079";
        static final String F80 = C60K + "b080";
        static final String F81 = C60K + "b081";
        static final String F82 = C60K + "b082";
        static final String F83 = C60K + "b083";
        static final String F84 = C60K + "b084";
        static final String F85 = C60K + "b085";
        static final String F86 = C60K + "b086";
        static final String F87 = C60K + "b087";
        static final String F88 = C60K + "b088";
        static final String F89 = C60K + "b089";
        static final String F90 = C60K + "b090";
        static final String F91 = C60K + "b091";
        static final String F92 = C60K + "b092";
        static final String F93 = C60K + "b093";
        static final String F94 = C60K + "b094";
        static final String F95 = C60K + "b095";
        static final String F96 = C60K + "b096";
        static final String F97 = C60K + "b097";
        static final String F98 = C60K + "b098";
        static final String F99 = C60K + "b099";
        static final String F100 = C60K + "b100";
        static final String F101 = C60K + "b101";
        static final String F102 = C60K + "b102";
        static final String F103 = C60K + "b103";
        static final String F104 = C60K + "b104";
        static final String F105 = C60K + "b105";
        static final String F106 = C60K + "b106";
        static final String F107 = C60K + "b107";
        static final String F108 = C60K + "b108";
        static final String F109 = C60K + "b109";
        static final String F110 = C60K + "b110";
        static final String F111 = C60K + "b111";
        static final String F112 = C60K + "b112";
        static final String F113 = C60K + "b113";
        static final String F114 = C60K + "b114";
        static final String F115 = C60K + "b115";
        static final String F116 = C60K + "b116";
        static final String F117 = C60K + "b117";
        static final String F118 = C60K + "b118";
        static final String F119 = C60K + "b119";
        static final String F120 = C60K + "b120";
        static final String F121 = C60K + "b121";
        static final String F122 = C60K + "b122";
        static final String F123 = C60K + "b123";
        static final String F124 = C60K + "b124";
        static final String F125 = C60K + "b125";
        static final String F126 = C60K + "b126";
        static final String F127 = C60K + "b127";
        static final String F128 = C60K + "b128";
        static final String F129 = C60K + "b129";
        static final String F130 = C60K + "b130";
        static final String F131 = C60K + "b131";
        static final String F132 = C60K + "b132";
        static final String F133 = C60K + "b133";
        static final String F134 = C60K + "b134";
        static final String F135 = C60K + "b135";
        static final String F136 = C60K + "b136";
        static final String F137 = C60K + "b137";
        static final String F138 = C60K + "b138";
        static final String F139 = C60K + "b139";
        static final String F140 = C60K + "b140";
        static final String F141 = C60K + "b141";
        static final String F142 = C60K + "b142";
        static final String F143 = C60K + "b143";
        static final String F144 = C60K + "b144";
        static final String F145 = C60K + "b145";
        static final String F146 = C60K + "b146";
        static final String F147 = C60K + "b147";
        static final String F148 = C60K + "b148";
        static final String F149 = C60K + "b149";
        static final String F150 = C60K + "b150";
        static final String F151 = C60K + "b151";
        static final String F152 = C60K + "b152";
        static final String F153 = C60K + "b153";
        static final String F154 = C60K + "b154";
        static final String F155 = C60K + "b155";
        static final String F156 = C60K + "b156";
        static final String F157 = C60K + "b157";
        static final String F158 = C60K + "b158";
        static final String F159 = C60K + "b159";
        static final String F160 = C60K + "b160";
        static final String F161 = C60K + "b161";
        static final String F162 = C60K + "b162";
        static final String F163 = C60K + "b163";
        static final String F164 = C60K + "b164";
        static final String F165 = C60K + "b165";
        static final String F166 = C60K + "b166";
        static final String F167 = C60K + "b167";
        static final String F168 = C60K + "b168";
        static final String F169 = C60K + "b169";
        static final String F170 = C60K + "b170";
        static final String F171 = C60K + "b171";
        static final String F172 = C60K + "b172";
        static final String F173 = C60K + "b173";
        static final String F174 = C60K + "b174";
        static final String F175 = C60K + "b175";
        static final String F176 = C60K + "b176";
        static final String F177 = C60K + "b177";
        static final String F178 = C60K + "b178";
        static final String F179 = C60K + "b179";
        static final String F180 = C60K + "b180";
        static final String F181 = C60K + "b181";
        static final String F182 = C60K + "b182";
        static final String F183 = C60K + "b183";
        static final String F184 = C60K + "b184";
        static final String F185 = C60K + "b185";
        static final String F186 = C60K + "b186";
        static final String F187 = C60K + "b187";
        static final String F188 = C60K + "b188";
        static final String F189 = C60K + "b189";
        static final String F190 = C60K + "b190";
        static final String F191 = C60K + "b191";
        static final String F192 = C60K + "b192";
        static final String F193 = C60K + "b193";
        static final String F194 = C60K + "b194";
        static final String F195 = C60K + "b195";
        static final String F196 = C60K + "b196";
        static final String F197 = C60K + "b197";
        static final String F198 = C60K + "b198";
        static final String F199 = C60K + "b199";
        static final String F200 = C60K + "b200";
        static final String F201 = C60K + "b201";
        static final String F202 = C60K + "b202";
        static final String F203 = C60K + "b203";
        static final String F204 = C60K + "b204";
        static final String F205 = C60K + "b205";
        static final String F206 = C60K + "b206";
        static final String F207 = C60K + "b207";
        static final String F208 = C60K + "b208";
        static final String F209 = C60K + "b209";
        static final String F210 = C60K + "b210";
        static final String F211 = C60K + "b211";
        static final String F212 = C60K + "b212";
        static final String F213 = C60K + "b213";
        static final String F214 = C60K + "b214";
        static final String F215 = C60K + "b215";
        static final String F216 = C60K + "b216";
        static final String F217 = C60K + "b217";
        static final String F218 = C60K + "b218";
        static final String F219 = C60K + "b219";
        static final String F220 = C60K + "b220";
        static final String F221 = C60K + "b221";
        static final String F222 = C60K + "b222";
        static final String F223 = C60K + "b223";
        static final String F224 = C60K + "b224";
        static final String F225 = C60K + "b225";
        static final String F226 = C60K + "b226";
        static final String F227 = C60K + "b227";
        static final String F228 = C60K + "b228";
        static final String F229 = C60K + "b229";
        static final String F230 = C60K + "b230";
        static final String F231 = C60K + "b231";
        static final String F232 = C60K + "b232";
        static final String F233 = C60K + "b233";
        static final String F234 = C60K + "b234";
        static final String F235 = C60K + "b235";
        static final String F236 = C60K + "b236";
        static final String F237 = C60K + "b237";
        static final String F238 = C60K + "b238";
        static final String F239 = C60K + "b239";
        static final String F240 = C60K + "b240";
        static final String F241 = C60K + "b241";
        static final String F242 = C60K + "b242";
        static final String F243 = C60K + "b243";
        static final String F244 = C60K + "b244";
        static final String F245 = C60K + "b245";
        static final String F246 = C60K + "b246";
        static final String F247 = C60K + "b247";
        static final String F248 = C60K + "b248";
        static final String F249 = C60K + "b249";
        static final String F250 = C60K + "b250";
        static final String F251 = C60K + "b251";
        static final String F252 = C60K + "b252";
        static final String F253 = C60K + "b253";
        static final String F254 = C60K + "b254";
        static final String F255 = C60K + "b255";
    }

    /** Fill the heap with live chunks; they die when this returns. Answers the chunks held. */
    static int fill() {
        Object[] head = null;
        int chunks = 0;
        int[] rungs = {16 * 1024, 1024, 256};
        for (int r = 0; r < rungs.length; r++) {
            int words = rungs[r];
            try {
                while (true) {
                    Object[] node = new Object[2];
                    node[0] = head;
                    node[1] = new long[words];
                    head = node;
                    chunks++;
                }
            } catch (OutOfMemoryError e) {
                // This rung is full; `head` holds every chunk until return.
            }
        }
        return head == null ? -1 : chunks;
    }

    public static void main(String[] args) throws Exception {
        ClassLoader loader = GciRf38WideReflectLinkOomeProbe.class.getClassLoader();
        boolean diag = args.length > 0 && args[0].equals("diag");
        Runtime rt = Runtime.getRuntime();
        if (diag) {
            System.err.println("[gci-rf38] free-at-start=" + rt.freeMemory());
        }
        for (int i = 0; i < 2; i++) {
            // Loaded, neither linked nor initialized.
            Class<?> c = Class.forName("GciRf38WideReflectLinkOomeProbe$Wide" + i, false, loader);
            System.err.println("[gci-rf38] stage " + i + " fill-begin");
            int chunks = fill();
            long freeAtCall = diag ? rt.freeMemory() : -1L;
            // The heap is full of garbage now; the listing is the next thing to run.
            Field[] fields = null;
            int code = 0;
            try {
                fields = c.getDeclaredFields();
            } catch (OutOfMemoryError e) {
                code = 1;
            } catch (Throwable t) {
                code = 2;
            }
            long sum = -1;
            if (fields != null) {
                sum = 0;
                for (Field f : fields) {
                    sum += ((String) f.get(null)).length();
                }
            }
            String codeWord = code == 0 ? "ok" : (code == 1 ? "oome" : "other");
            System.out.println("stage " + i + " fill=" + (chunks > 0 ? "oome" : "none")
                    + " getDeclaredFields=" + codeWord
                    + " fields=" + (fields == null ? -1 : fields.length)
                    + " sum=" + sum);
            System.err.println("[gci-rf38] stage " + i + " chunks=" + chunks
                    + " free-at-call=" + freeAtCall);
        }
        System.out.println("PROBE-END");
    }
}
