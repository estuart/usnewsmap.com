# Page-language samples

`samples.jsonl` is text of known language for the page-language detector's ground-truth test (`crates/usnm-core/tests/page_language_samples.rs`, 05 §5.5.8, #289): for each catalog language with a word list (`usnm_core::lang`; Croatian and Serbian share `sh` and each have their own samples), about the first 300 words of the running prose of three Wikipedia articles and three Wikisource texts. Unlike the rest of `fixtures/`, it is real text, and it keeps the licences of its sources.

| Samples    | Source                                                                                                      | Licence                                                  |
| ---------- | ----------------------------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| Wikipedia  | articles on general and historical subjects, from each language's Wikipedia                                 | CC BY-SA 4.0, Wikipedia contributors                     |
| Wikisource | works published in 1930 or earlier, from the language's Wikisource or the multilingual one (wikisource.org) | public domain in the US (published 95 or more years ago) |

## The samples

One JSON object a line:

- `language`: the MARC code the detector reports (`ger`, `hrv`); `wordlist`: its word list (`de`, `sh`);
- `source` (`wikipedia` or `wikisource`), `wiki` (the domain), `title` (the page read) and `revid`, the revision read; `url` is that revision's permalink;
- for Wikisource: `work` (the Wikidata item, if it has one), `author`, `year` (of publication) and `year_source` (where it comes from: the item's publication date, P577, for all but the few works Wikidata doesn't date), `decade`, `median_decade` (the language's in our corpus: the decade where its pages in the OCR audit's `by_language_decade` table, `docs/reports/2026-10-language-analysis/data/ocr-quality-v2-10pct.jsonl`, reach half; the corpus's own for a language with fewer than 50 pages there), `target_decade` (the decade the script looked in first), `decade_note` (why they differ) and `copyright`;
- for Wikipedia: `licence`;
- `words` and `text`: the prose, a paragraph a line. Only `<p>` paragraphs of 6 words or more are kept, without headings, references, tables, lists, figures, quotations, poems, page numbers or templates' boxes, and the last is cut at the end of the sentence that reaches 300 words;
- `expect` (added by hand): where the detector can't tell the sample's language from another on this text, the candidate languages (in title order), the result it gives (a code, `und`, or `mixed` and two codes) and why. The test checks that result instead of the sample's language, so a change shows either way.

## Wikipedia excerpts

The Wikipedia excerpts are shared under the Creative Commons Attribution-ShareAlike 4.0 licence (<https://creativecommons.org/licenses/by-sa/4.0/>). Attribution: Wikipedia contributors. Each sample names its article and the revision it was taken from (`wiki`, `title`, `url`), whose history lists the authors. They are cut from those revisions as described above, which is the only change.

## Wikisource texts

Each Wikisource text was published in 1930 or earlier, so it is in the public domain in the US. Some are translations; their year is the translation's, from the Wikidata item of that edition. A language's texts come from its median decade in our corpus if that is the 1920s or earlier, and from the 1920s (to 1930) if it is later; when Wikisource has too few texts of that decade with an author, a sourced date and enough prose, from the nearest decade that has them. Each sample's `decade_note` says which. They are:

<!-- The list below is written by scripts/page-language-samples.py. -->

- `ara`: الأدب الكبير/مقدمة, by عبد الله بن المقفع, 1912 (ar.wikisource.org)
- `ara`: مجلة البيان للبرقوقي/العدد 50/ملكة الورق, by ألكسندر بوشكين, 1919 (ar.wikisource.org)
- `ara`: مفيد العلوم ومبيد الهموم/خطبة الكتاب, by محمد بن العباس الخوارزمي, 1906 (ar.wikisource.org)
- `bul`: Легенди при Царевец/Предговор, by Иван Вазов, 1910 (bg.wikisource.org)
- `bul`: Природата като елемент на щастие, by Гунчо Гунчев, 1920 (bg.wikisource.org)
- `bul`: Рапортьор, by Христо Смирненски, 1919 (bg.wikisource.org)
- `cze`: K. H. Mácha a jeho dědictví, by František Xaver Šalda, 1910 (cs.wikisource.org)
- `cze`: Ke dni Máchovu, by Jindřich Vodák, 1910 (cs.wikisource.org)
- `cze`: Pozoruhodný případ Davidsonových očí/Pozoruhodný případ Davidsonových očí, by Herbert George Wells, 1910 (cs.wikisource.org)
- `dan`: Det graa Hus, by Herman Bang, 1901 (da.wikisource.org)
- `dan`: Fra Bregnegaard og Omegn, by Cornelia von Levetzow, 1900 (da.wikisource.org)
- `dan`: Julegæster fra havet, by Ludvig Mylius-Erichsen, 1901 (da.wikisource.org)
- `dut`: Door Duisternis Tot Licht/Inleiding, by Kartini, 1911 (nl.wikisource.org)
- `dut`: Het leven der bloem (1900)/I, by Hugo de Vries, 1900 (nl.wikisource.org)
- `dut`: Pallieter/I, by Felix Timmermans, 1916 (nl.wikisource.org)
- `eng`: A Book of Dartmoor, by Sabine Baring-Gould, 1900 (en.wikisource.org)
- `eng`: A Chambermaid's Diary, by Octave Mirbeau, 1900 (en.wikisource.org)
- `eng`: A History of Sanskrit Literature/Preface, by Arthur Anthony Macdonell, 1900 (en.wikisource.org)
- `fin`: Kiljusen herrasväki/Kiljusen herrasväen Helsinginmatka, by Jalmari Finne, 1914 (fi.wikisource.org)
- `fin`: Laguksen asiakirjakokoelma (1910), by E. G. Palmén, 1910 (fi.wikisource.org)
- `fin`: Älykkään ritarin don Quijote de la Manchan elämänvaiheet/Ensimäinen luku, by Miguel de Cervantes, 1905 (fi.wikisource.org)
- `fre`: Adorable Clio/Nuit à Châteauroux, by Jean Giraudoux, 1920 (fr.wikisource.org)
- `fre`: Album de vers anciens (1920)/Texte entier, by Paul Valéry, 1920 (fr.wikisource.org)
- `fre`: Alcibiade (trad. Croiset), by Platon, 1920 (fr.wikisource.org)
- `ger`: Chemnitz am Ende des XIX Jahrhunderts in Wort und Bild/Einleitung, by Curt Wilhelm Zöllner, 1900 (de.wikisource.org)
- `ger`: Der korrekte Diener, by Heinrich XXVIII. Reuß zu Köstritz, 1900 (de.wikisource.org)
- `ger`: Der „dialogus miraculorum“ des Cäsarius von Heisterbach in seinen Beziehungen zu Aachen, by Johannes Pschmadt, 1900 (de.wikisource.org)
- `gre`: Παραμύθι χωρίς όνομα/Κεφάλαιο Α, by Πηνελόπη Δέλτα, 1910 (el.wikisource.org)
- `gre`: Στρίγγλα Μάννα, by Αλέξανδρος Παπαδιαμάντης, 1902 (el.wikisource.org)
- `gre`: Τα ψηλά βουνά/Οι τρεις φωτιές, by Ζαχαρίας Παπαντωνίου, 1918 (el.wikisource.org)
- `heb`: אוצר מדרשים/מדרש מעשה חנוכה, by יהודה דוד אייזנשטיין, 1915 (he.wikisource.org)
- `heb`: לקורות גדולי ליטא, by בן-ציון אייזנשטט, 1900 (he.wikisource.org)
- `heb`: תולדות תנאים ואמוראים/הקדמה, by אהרן הימן, 1910 (he.wikisource.org)
- `hrv`: Kako je Potjeh tražio istinu/I, by Ivana Brlić-Mažuranić, 1916 (hr.wikisource.org)
- `hrv`: Zlatarevo zlato, by August Šenoa, 1871 (hr.wikisource.org)
- `hrv`: Čudnovate zgode šegrta Hlapića/Kod majstora Mrkonje/I, by Ivana Brlić-Mažuranić, 1913 (hr.wikisource.org)
- `hun`: A legsötétebb Európa, by Nopcsa Ferenc, 1911 (hu.wikisource.org)
- `hun`: Egri csillagok/1. rész, by Gárdonyi Géza, 1901 (hu.wikisource.org)
- `hun`: Tanár úr kérem/Bevezetés, by Karinthy Frigyes, 1916 (hu.wikisource.org)
- `ice`: Tómas frændi/I, by Harriet Beecher Stowe, 1901 (is.wikisource.org)
- `ita`: Dal mio verziere/A Elda Gianelli, by Maria Majocchi, 1910 (it.wikisource.org)
- `ita`: I Manifesti del futurismo/Contro Venezia passatista, by Filippo Tommaso Marinetti, 1910 (it.wikisource.org)
- `ita`: La conquista di Roma/Parte prima, by Matilde Serao, 1910 (it.wikisource.org)
- `lav`: 1.daļa (Mērnieku laiki), by Matīss Kaudzīte, Reinis Kaudzīte, 1879 (wikisource.org)
- `lav`: Zelta zirgs, by Rainis, 1909 (wikisource.org)
- `lit`: Kunigas, by Juozapas Ignotas Kraševskis, 1887 (lt.wikisource.org)
- `lit`: Liūdna pasaka, by Jonas Biliūnas, 1907 (lt.wikisource.org)
- `nor`: Da Kristiania var smaaby/1, by Edvard Mørch, 1901 (no.wikisource.org)
- `nor`: En Tale om Maalsagen, by Bjørnstjerne Bjørnson, 1900 (no.wikisource.org)
- `nor`: Madame Telliers hus/01, by Guy de Maupassant, 1900 (no.wikisource.org)
- `pol`: Apologia Sokratesa (tłum. Siedlecki), by Platon, 1910 (pl.wikisource.org)
- `pol`: Cmentarna lilja/Rozdział I, by Selma Lagerlöf, 1910 (pl.wikisource.org)
- `pol`: Gloria victis (nowela), by Eliza Orzeszkowa, 1910 (pl.wikisource.org)
- `por`: A Alma Encantadora das Ruas/A Rua, by João do Rio, 1910 (pt.wikisource.org)
- `por`: Lendas do Sul/A M'boi-tátá, by João Simões Lopes Neto, 1913 (pt.wikisource.org)
- `por`: Para ler nas férias/A desobediente, by Maria O'Neill, 1913 (pt.wikisource.org)
- `rum`: Daphnis și Chloe/Introducere, by Longus, 1922 (ro.wikisource.org)
- `rum`: Ion (Rebreanu)/1. Glasul pământului, by Liviu Rebreanu, 1920 (ro.wikisource.org)
- `rum`: Macbeth/Notă introductivă, by William Shakespeare, 1922 (ro.wikisource.org)
- `rus`: В кафе (Булгаков), by Михаил Афанасьевич Булгаков, 1920 (ru.wikisource.org)
- `rus`: Вне Земли (Циолковский), by Константин Циолковский, 1920 (ru.wikisource.org)
- `rus`: Задачи союзов молодёжи (Ленин), by Владимир Ильич Ленин, 1920 (ru.wikisource.org)
- `slo`: Dva dni v Chujave, by Jonáš Záborský, 1873 (sk.wikisource.org)
- `slv`: Carski sel, by Jules Verne, 1923 (sl.wikisource.org)
- `slv`: Hlapci, by Ivan Cankar, 1910 (sl.wikisource.org)
- `slv`: Za kruhom, by Henryk Sienkiewicz, 1924 (sl.wikisource.org)
- `spa`: Amadeo I/I, by Benito Pérez Galdós, 1910 (es.wikisource.org)
- `spa`: Ariel/Capítulo I, by José Enrique Rodó, 1910 (es.wikisource.org)
- `spa`: Del porvenir del castellano en Filipinas, by Wenceslao Retana, 1910 (es.wikisource.org)
- `srp`: Вођа, by Радоје Домановић, 1901 (sr.wikisource.org)
- `srp`: Лажни цар Шћепан Мали/1, by Петар II Петровић Његош, 1851 (sr.wikisource.org)
- `srp`: Нечиста крв/Глава прва, by Борисав Станковић, 1910 (sr.wikisource.org)
- `swe`: 19de Århundradet Förra Delen/Inledning, by Oscar Heinrich Dumrath, 1902 (sv.wikisource.org)
- `swe`: Arfvingen till Redclyffe/Kapitel 01, by Charlotte Mary Yonge, 1901 (sv.wikisource.org)
- `swe`: Socialpolitik/Socialism, liberalism och socialpolitik, by Gustav Cassel, 1902 (sv.wikisource.org)
- `tgl`: Banaag at Sikat/Paunawa, by Lope K. Santos, 1906 (tl.wikisource.org)
- `ukr`: Історія Слободської України, by Багалій Дмитро Іванович, 1918 (uk.wikisource.org)
- `ukr`: Історія України-Русі (Аркас, 1912)/Микола Аркас, by Аркас Микола Миколайович (старший), 1912 (uk.wikisource.org)
- `ukr`: Самостійна Україна (Міхновський, 1917), by Міхновський Микола Іванович, 1917 (uk.wikisource.org)
- `yid`: צווישן צוויי וועלטן (דער דיבוק), by ש. אַנ-סקי, 1922 (yi.wikisource.org)

<!-- End of the list. -->

## Languages with fewer samples

Every language has three Wikipedia excerpts. Wikisource has fewer than three texts with an author, a sourced year of publication by 1930 and 300 words of prose in these languages, at any decade; the test (`FEWER`) expects exactly these counts:

- Hawaiian (`haw`): none. There is no Hawaiian Wikisource, and the Hawaiian texts on the multilingual one are songs and chants, and the 1894 constitution, which is all articles and lists.
- Icelandic (`ice`): one. Icelandic Wikisource's other texts of the time (Jón Trausti's novels, for one) have no publication date in Wikidata or on the page.
- Latvian (`lav`) and Lithuanian (`lit`): two each, from the multilingual Wikisource and Lithuanian Wikisource.
- Slovak (`slo`): one. Slovak Wikisource has a handful of dated prose texts, most of them under 300 words.
- Tagalog (`tgl`): one.
- Yiddish (`yid`): one. Yiddish Wikisource's other texts of the time (I. L. Peretz's stories, for one) have no publication date in Wikidata.

## Regenerating

The script reads Wikidata's query service and the MediaWiki API, one request at a time with a descriptive User-Agent, waiting out rate limits, so a full run takes an hour or two. Regenerate some languages (the others' samples are kept, and so is a sample's `expect` when the same revision is chosen again), or check that every sample's revision still gives the same text:

```sh
scripts/page-language-samples.py --languages ger,fre
scripts/page-language-samples.py --check
```

MediaWiki renders an old revision with today's templates, and a Wikisource text transcluded from its scans' `Page:` pages with their current revisions, so `--check` can report a change the pinned revision doesn't explain; regenerate that language. Then run the test:

```sh
cargo test -p usnm-core --test page_language_samples -- --ignored
```

Nothing in CI runs the script: the samples are committed, and the test reads only them.
