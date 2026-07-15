# Project Archive (`parc`)

**Eski geliştirme projelerini kaybetmeden küçült ve arşivle.**

`parc` geliştirme projelerini bulur, yeniden üretilebilen kısımları ayıklar (`node_modules`, `target`, `.venv`, `Pods`...) ve kalanı tek bir sıkıştırılmış dosyaya, her şeyi geri getirecek bir reçeteyle birlikte arşivler. Bir disk temizleyici değildir: amaç yer açmak değil, **projeyi kaybetmeden küçültmektir**.

[English](README.md) | **Türkçe**

---

## Sorun

Bir `~/Projects` klasörü aylar önce bitirdiğin işlerle dolar. Silmek içine sinmez: bazıları bir remote'a gönderilmiştir, ama birçoğu başka hiçbir yerde olmayan bir `.env`, yerel bir veritabanı, yüklenmiş dosyalar veya commit'lenmemiş iş taşır. Böylece öylece dururlar, ve neredeyse her birindeki en büyük şey tek komutla yeniden kurabileceğin bir `node_modules` veya `target`'tır.

`parc` bu iki gerçeği ayırır. Bir lockfile'ın ya da bir derleme komutunun yeniden üretebildiği şey silinir ve sonra çalıştırılacak bir adım olarak yazılır. Sadece diskte var olan şey ise korunur. Sonuç, çalışır bir projeye geri açabileceğin küçük bir arşivdir.

## Kurulum

Rust ile yazılmıştır. Repo kökünden:

```bash
cargo install --path .        # `parc`'ı ~/.cargo/bin içine kurar
# ya da sadece derle:
cargo build --release         # ikili: ./target/release/parc
```

## Hızlı başlangıç

```bash
parc scan ~/Projects          # ne kazanılabilir, salt okunur
parc backup ~/Projects        # hepsini arşivle, hiçbir projeye dokunma
parc archive ~/Projects/eski-sey   # arşivle, doğrula, sonra orijinali çöpe at
parc list                     # arşiv kütüphanen
parc restore eski-sey         # aç ve ayıklananları geri kur
```

Arşivler varsayılan olarak `~/Archives` altına yazılır (`--out` veya `PARC_ARCHIVE_DIR` ortam değişkeni ile değiştirilir).

## Komutlar

```
parc scan <dizin>        tara, ne kazanılabileceğini raporla         (salt okunur)
parc plan <proje>        ne arşive girer, ne girmez                  (salt okunur)
parc clean <dizin>       node_modules/dist sil, projeyi bırak        (arşivlemez)
parc backup <dizin>      hepsini arşivle, hiçbir projeye dokunma
parc archive <proje>     arşivle, doğrula, orijinali çöpe at         (önce sorar)
parc verify <arşiv>      her dosyanın sha256'sını yeniden hesapla
parc show <arşiv>        bu neydi: ne silindi, ne lazım              (salt okunur)
parc restore <arşiv>     aç + ayıklananları geri kur (install, generate...)
parc list                arşiv kütüphanesi
parc rm <arşiv>          arşivi kütüphaneden çıkar (çöpe taşır)
```

`scan` ve `clean`, `--older-than <gün>` alır, örn. `parc clean ~/Projects --older-than 180`.

## Dosyalarına dokunan dört komut (ve farkları)

Üç ayrı soruya üç ayrı cevap. Ayıran şey **projene ne olduğudur**:

| Komut           | Projene ne olur                            | Arşiv yazar mı |
|-----------------|--------------------------------------------|----------------|
| `scan`, `plan`  | hiçbir şey                                 | hayır          |
| `backup`        | **hiçbir şey**                             | evet           |
| `clean`         | üretilmiş klasörler silinir, gerisi durur  | hayır          |
| `archive`       | **çöp kutusuna taşınır**                    | evet           |

- **`clean`** hâlâ üzerinde çalıştığın projeler içindir. `node_modules`, `dist`, `target` gider; kaynak, `.env`, `.git` ve lockfile'lar kalır. Proje çalışmaya devam eder, `pnpm install` her şeyi geri getirir. Yer kazancının büyük kısmı buradadır ve arşiv gerekmez: silinen her şey bir lockfile'dan yeniden kurulabilir.
- **`backup`** bir dizinin altındaki her projeyi bulur ve tek tek arşivler. Projelere asla dokunmaz, sadece küçültülmüş bir kopyasını rafa kaldırır. Silme bayrağı yoktur, silme kod yolu da yoktur. Her hafta çalıştırabilirsin.
- **`archive`** işin bittiği projeler içindir: arşivler, arşivi diskten geri okuyup her baytını doğrular ve **ancak o zaman** orijinali çöp kutusuna taşır. Başında "devam?" diye sorar (`--yes` ile atlanır). Bu komut projeyi diskinden kaldırır, istenen budur.

Yıkıcı olan iki komut, `clean` ve `archive`, sormadan hiçbir şey yapmaz. `scan`, `plan` ve `backup` ise hiçbir koşulda projene dokunmaz.

## Ne silinir, nasıl geri gelir

Her arşiv, neyin ayıklandığını ve onu neyin geri getirdiğini **kendi içinde taşır** (`manifest.json` -> `removed` + `restore_plan`). Bunu üç yerde görürsün: `archive` biterken, `parc show <arşiv>` ile istediğin an, ve `restore` sırasında.

```
$ parc show acme-backend
  acme-backend
  geldiği yer    ~/Desktop/acme/backend
  stack          node, nestjs, react  ·  pnpm
  runtime        node >=20
  boyut          546 KB arşiv  ·  727 MB orijinal

  silinenler (720 MB):
    node_modules     720 MB  kural: node_modules

  geri getirmek için:
    1. pnpm install            -> node_modules (720 MB)
    2. pnpm prisma generate    -> Prisma client  (opsiyonel)
```

**`restore` bu adımları senin için çalıştırır** - ek bir bayrak gerekmez. Geri açılmış proje çalışmaya hazırdır, "bu neydi, ne kurmam lazımdı?" diye uğraşmazsın. Sadece dosyaları alıp komutları atlamak için: `--no-setup`.

Reçete **arşiv anında** yazılır, çünkü lockfile'ı, `package.json` script'lerini, Podfile'ı ve tam olarak neyin silindiğini sadece o an görebiliriz. Lockfile'lar asla silinmez - lockfile, `pnpm install`'ı bir umuttan bir söze çeviren şeydir. Sadece ondan yeniden üretilebilen şey silinir (`node_modules`, `dist`, `target`, `Pods`, `.venv`, `vendor`...).

Stack başına ne temizlenir, neyle geri gelir:

| Stack   | Temizlenen                           | Geri getiren                             |
|---------|--------------------------------------|------------------------------------------|
| Node    | node_modules, .next, dist, .turbo... | `pnpm install` (+ prisma generate, build)|
| Rust    | target/                              | `cargo build`                            |
| Python  | .venv, __pycache__...                | `uv sync` / `poetry install` / venv+pip  |
| PHP     | vendor/                              | `composer install`                       |
| Ruby    | vendor/                              | `bundle install`                         |
| Go      | vendor/                              | `go mod vendor`                          |
| Flutter | .dart_tool, build/                   | `flutter pub get`                        |
| iOS     | Pods/, DerivedData                   | `pod install`                            |

**Üretilmiş, yeniden üretilebilir demek değildir.** Bazı klasörler bilerek listede yoktur: üretilmişlerdir ama onları kaynaktan geri kuran bir komut yoktur, o yüzden silmek artefakt kılığında veri kaybı olurdu. `log/` (geçen yılın production logu), `tmp/` (Rails cache'leri ve pid'leri, ama aynı zamanda ActiveStorage'ın `tmp/storage` içindeki yüklenmiş blob'ları), `.wrangler/state` (yerel D1 veritabanı, KV, R2, aylarca seed'lenmiş veri), `.vercel/` (bir dizini Vercel projesine bağlayan link). Bunlar tam olarak arşivin var olma sebebidir, o yüzden silme adayı değil **arşive-özel** dosyalar olarak görünürler.

Zorunlu bir adım patlarsa (iki yıllık bir lockfile bugün çözülmeyebilir) `restore` bunu söyler ve durur: **kaynak eksiksiz açılmıştır**, kaybolan bir şey yoktur.

## Geri yükleme

```bash
parc restore <arşiv>              # aç, sonra reçeteyi çalıştır (nereye diye sorar)
parc restore <arşiv> --original   # geldiği yere, sorusuz
parc restore <arşiv> --into <dizin> # bu dizinin altına, sorusuz
parc restore <arşiv> --no-setup   # sadece dosyalar, komutları atla
```

Klasör her zaman **orijinal adıyla** geri gelir (`health`), arşivin dosya adıyla değil (`acme-health`). Bir boru hattında veya script'te soru atlanır ve bulunduğun dizine açılır.

## Şifreleme

Bir arşiv, git'in tutmayı reddettiği dosyaları taşır - `.env`, servis anahtarları, `secrets/`. Bir `git clone`'dan üstün olmasının sebebi tam olarak budur, ama aynı sebeple diskteki dosya projenin bütün sırlarının düz metin kopyasıdır. Önemsediklerin için:

```bash
parc archive <proje> --encrypt      # parola sorar (iki kez)
```

`--encrypt` hem `backup`'ta hem `archive`'da çalışır. [age](https://age-encryption.org) formatını (parola tabanlı, scrypt) kullanarak `<isim>.parc.tar.zst.age` üretir. Uydurma bir format değildir: `parc` ortadan kaybolsa bile `age -d proje.parc.tar.zst.age | tar --zstd -x` her şeyi geri verir. Şifreleme en dış katmandır (tar -> zstd -> age), yani çözülen şey sıradan bir `.tar.zst`'dir.

Şifreli olsun ya da olmasın, arşiv dosyası `0600` yazılır (sadece sahibi okur): şifresiz bir arşiv projenin tuttuğu her sırrın kopyasıdır ve makinedeki başka bir hesabın onu okumak için bir sebebi yoktur.

`show`, `verify` ve `restore` parola sorar; script'lerde `PARC_PASSPHRASE` ile verilir. **`list` asla sormaz** - bir kütüphane farklı parolalı arşivler tutabilir. Bunun bedeli: şifreli bir arşiv `list`'te boyutundan başka hiçbir şey göstermez.

> **Parolayı kaybedersen proje gider.** Kurtarma yolu yoktur, olmaması da şifrelemenin tanımıdır.

## Arşiv formatı

`<isim>.parc.tar.zst` - düz bir tar.zst, özel bir format değil. `parc` ortadan kaybolsa bile `tar --zstd -xf` onu açar. İlk kayıt `.parc/manifest.json`'dır: her dosyanın sha256'sı, ne silindiği, git durumu, stack, paket yöneticisi.

İsim `<son-iki-dizin>-<parmak-izi>` biçimindedir, örn. `app-backend-a3f19c`. Okunur kısım projenin nerede durduğunu söyler; parmak izi yolunun bir hash'idir, böylece `acme/app/backend` ile `globex/app/backend` aynı dosya olmaz. Parmak izini yazmak zorunda değilsin - `parc restore app-backend` tek bir arşive denk geldiği sürece onu bulur.

Bir projeyi tekrar arşivlemek, yanına ikinci bir kopya koymak yerine kendi arşivinin üzerine yazar (`--overwrite`). Eskisi **çöp kutusuna** gider, asla doğrudan silinmez.

## Arşiv silmek

`parc rm <arşiv>` bir arşivi kütüphaneden çıkarır - dosyayı silmez, çöp kutusuna taşır. `list` bir arşivi **arşive-özel** diye işaretliyorsa, o projenin kalan tek kopyası odur, ve `rm` reddeder:

```
$ parc rm acme-backend
Error: bu arşiv tek kopya - orijinali diskte yok:
    acme-backend  (546 KB)
      geldiği yer: ~/Desktop/acme/backend  - artık orada değil
  Önce `parc restore <ad>` ile geri yükle, ya da gerçekten istiyorsan --force ver.
```

`--yes` bunu **aşmaz**; sadece onay sorusunu atlar. Tek kopyayı silmek bilerek bir `--force` ister. Bir istekteki tek bir arşiv bile engellenirse hiçbiri silinmez.

## Neyin silinmesinin güvenli olduğuna nasıl karar verir

Tasarım tek satırda: **kurallar bir allowlist'tir, blocklist değil.** Bir klasör ancak adı [`rules.rs`](src/rules.rs)'teki tabloda geçiyorsa silme adayı olur. "Büyük" olmak asla bir sebep değildir.

- **`.gitignore` bir sinyaldir, dışlama listesi değil.** Tam olarak git'te *olmayan* şeyleri listeler - `.env`, yerel veritabanı, `uploads/`. Bunlar arşivin var olma sebebidir, atlanacak dosyalar değil.
- **Git artefaktı doğrular.** Git bir `dist/`'i ignore ediyorsa üretilmiştir, silinebilir. İçindekileri *takip ediyorsa* o kaynaktır ve dokunulmaz. İkisi de değilse karar insana kalır (`REVIEW`).
- **Geri getirecek bir şey yoksa hiçbir şey silinmez.** Her klasör için tek bir soru vardır: *bunu kim geri koyar?* Bir cache (kimse, araç yeniden doldurur), bir bağımlılık ağacı (onu sahiplenen lockfile) ya da derleme çıktısı (onu üreten komut). Cevap yoksa, silme yok.
- **Sıra pazarlıksızdır.** Temizliği simüle et -> arşivi yaz -> diskten geri okuyup her baytı yeniden hash'le -> ağacı ikinci kez gez ve karşılaştır -> ancak o zaman orijinale dokun.
- **Silmek `rm` değildir.** Orijinal çöp kutusuna gider. `parc rm` de bir arşivi çöpe taşır. Bir arşiv projenin son kopyası olabilir, o yüzden hiçbir arşiv doğrudan silinmez.

Buradaki her kural, aracın daha eski bir sürümünün gerçek bir projede yanlış yapmış olmasından doğdu. Test paketi (`cargo test`) tamamen güvenlik vakalarıdır - her biri, aracın bir daha asla yapmaması gereken bir hatayı kilitler.

## Karar (verdict)

| Verdict     | Anlamı                                                                   |
|-------------|--------------------------------------------------------------------------|
| `REDUNDANT` | Remote'da eksiksiz. Arşiv gerekmez - sil, `git clone` geri getirir.      |
| `ARCHIVE`   | Git'in kopyası olmayan bir şey içerir. Arşivlenmeden silinemez.          |
| `REVIEW`    | Otomatik karar verilemedi. `archive`, `--force` olmadan reddeder.        |

## Yol haritası

- `.parcignore` - proje bazında kural ezme
- `parc scan --older-than 180d` + launchd ile zamanlanmış arşivleme
- içerik-adresli dedup (aynı `.git`'i iki kez saklamamak)
- bulut hedefleri (S3 / R2)

## Lisans

MIT
