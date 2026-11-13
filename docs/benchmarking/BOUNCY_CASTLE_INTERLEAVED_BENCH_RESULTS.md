# Bouncy Castle Full Suite Interleaved Benchmark: CVM vs HotSpot

**Date**: 2026-10-31 07:41:30 UTC  
**Host**: AMD EPYC-Turin (16 vCPUs, 60 GB RAM, Linux x86_64)  
**Configurations**: CratonVM Default (`/root/tc-bin/cratonvm`) vs OpenJDK HotSpot 25  
**Methodology**: Class-by-class interleaved execution with alternating order (1 iteration per class, 30s timeout per class)  

### Summary Metrics

- **Total Test Classes Evaluated**: 1444
- **CratonVM PASS Count**: 1120 / 1444 (77.6%)
- **HotSpot 25 PASS Count**: 1153 / 1444 (79.8%)
- **Both Passed**: 1118
- **Median Ratio (CVM / HotSpot)**: 5.32x
- **Mean Ratio (CVM / HotSpot)**: 5.78x
- **Classes Where CVM Was Faster**: 9 / 1118

### Class-by-Class Results (First 50 Samples)

| # | Class Name | First Run | CVM Status | CVM (ms) | HotSpot Status | HotSpot (ms) | CVM / HS Ratio |
|---:|:---|:---:|:---:|---:|:---:|---:|---:|
| 1 | `org.bouncycastle.asn1.ASN1TimeFormatTest` | CVM | PASS | 698 ms | PASS | 126 ms | 5.54x |
| 2 | `org.bouncycastle.asn1.cmc.test.BodyPartIDTest` | HotSpot | PASS | 476 ms | PASS | 58 ms | 8.21x |
| 3 | `org.bouncycastle.asn1.cmc.test.BodyPartListTest` | CVM | PASS | 483 ms | PASS | 64 ms | 7.55x |
| 4 | `org.bouncycastle.asn1.cmc.test.BodyPartPathTest` | HotSpot | PASS | 476 ms | PASS | 63 ms | 7.56x |
| 5 | `org.bouncycastle.asn1.cmc.test.BodyPartReferenceTest` | CVM | PASS | 488 ms | PASS | 63 ms | 7.75x |
| 6 | `org.bouncycastle.asn1.cmc.test.CMCCertificationRequestTest` | HotSpot | PASS | 538 ms | PASS | 75 ms | 7.17x |
| 7 | `org.bouncycastle.asn1.cmc.test.CMCFailInfoTest` | CVM | PASS | 485 ms | PASS | 67 ms | 7.24x |
| 8 | `org.bouncycastle.asn1.cmc.test.CMCPublicationInfoTest` | HotSpot | PASS | 539 ms | PASS | 79 ms | 6.82x |
| 9 | `org.bouncycastle.asn1.cmc.test.CMCStatusInfoTest` | CVM | PASS | 681 ms | PASS | 96 ms | 7.09x |
| 10 | `org.bouncycastle.asn1.cmc.test.CMCStatusInfoV2Test` | HotSpot | PASS | 705 ms | PASS | 98 ms | 7.19x |
| 11 | `org.bouncycastle.asn1.cmc.test.CMCStatusTest` | CVM | PASS | 490 ms | PASS | 65 ms | 7.54x |
| 12 | `org.bouncycastle.asn1.cmc.test.CMCUnsignedDataTest` | HotSpot | PASS | 450 ms | PASS | 59 ms | 7.63x |
| 13 | `org.bouncycastle.asn1.cmc.test.ControlsProcessedTest` | CVM | PASS | 470 ms | PASS | 63 ms | 7.46x |
| 14 | `org.bouncycastle.asn1.cmc.test.DecryptedPOPTest` | HotSpot | PASS | 472 ms | PASS | 64 ms | 7.38x |
| 15 | `org.bouncycastle.asn1.cmc.test.EncryptedPOPTest` | CVM | PASS | 503 ms | PASS | 71 ms | 7.08x |
| 16 | `org.bouncycastle.asn1.cmc.test.ExtendedFailInfoTest` | HotSpot | PASS | 483 ms | PASS | 64 ms | 7.55x |
| 17 | `org.bouncycastle.asn1.cmc.test.ExtensionReqTest` | CVM | PASS | 510 ms | PASS | 69 ms | 7.39x |
| 18 | `org.bouncycastle.asn1.cmc.test.GetCRLTest` | HotSpot | PASS | 702 ms | PASS | 95 ms | 7.39x |
| 19 | `org.bouncycastle.asn1.cmc.test.GetCertTest` | CVM | PASS | 486 ms | PASS | 64 ms | 7.59x |
| 20 | `org.bouncycastle.asn1.cmc.test.IdentityProofV2Test` | HotSpot | PASS | 509 ms | PASS | 80 ms | 6.36x |
| 21 | `org.bouncycastle.asn1.cmc.test.LraPopWitnessTest` | CVM | PASS | 485 ms | PASS | 64 ms | 7.58x |
| 22 | `org.bouncycastle.asn1.cmc.test.ModCertTemplateTest` | HotSpot | PASS | 494 ms | PASS | 71 ms | 6.96x |
| 23 | `org.bouncycastle.asn1.cmc.test.OtherMsgTest` | CVM | PASS | 492 ms | PASS | 67 ms | 7.34x |
| 24 | `org.bouncycastle.asn1.cmc.test.OtherStatusInfoTest` | HotSpot | PASS | 693 ms | PASS | 102 ms | 6.79x |
| 25 | `org.bouncycastle.asn1.cmc.test.PKIDataTest` | CVM | PASS | 522 ms | PASS | 78 ms | 6.69x |
| 26 | `org.bouncycastle.asn1.cmc.test.PKIResponseTest` | HotSpot | PASS | 483 ms | PASS | 71 ms | 6.8x |
| 27 | `org.bouncycastle.asn1.cmc.test.PendInfoTest` | CVM | PASS | 658 ms | PASS | 89 ms | 7.39x |
| 28 | `org.bouncycastle.asn1.cmc.test.PopLinkWitnessV2Test` | HotSpot | PASS | 478 ms | PASS | 66 ms | 7.24x |
| 29 | `org.bouncycastle.asn1.cmc.test.PublishTrustAnchorsTest` | CVM | PASS | 500 ms | PASS | 68 ms | 7.35x |
| 30 | `org.bouncycastle.asn1.cmc.test.RevokeRequestTest` | HotSpot | PASS | 696 ms | PASS | 107 ms | 6.5x |
| 31 | `org.bouncycastle.asn1.cmc.test.TaggedAttributeTest` | CVM | PASS | 483 ms | PASS | 66 ms | 7.32x |
| 32 | `org.bouncycastle.asn1.cmc.test.TaggedCertificationRequestTest` | HotSpot | PASS | 518 ms | PASS | 73 ms | 7.1x |
| 33 | `org.bouncycastle.asn1.cmc.test.TaggedContentInfoTest` | CVM | PASS | 493 ms | PASS | 71 ms | 6.94x |
| 34 | `org.bouncycastle.asn1.cmc.test.TaggedRequestTest` | HotSpot | PASS | 536 ms | PASS | 78 ms | 6.87x |
| 35 | `org.bouncycastle.asn1.cmp.test.CMPSequenceFieldOrderTest` | CVM | PASS | 828 ms | PASS | 118 ms | 7.02x |
| 36 | `org.bouncycastle.asn1.cmp.test.CertifiedKeyPairTest` | HotSpot | PASS | 567 ms | PASS | 82 ms | 6.91x |
| 37 | `org.bouncycastle.asn1.cmp.test.PKIFailureInfoTest` | CVM | PASS | 486 ms | PASS | 66 ms | 7.36x |
| 38 | `org.bouncycastle.asn1.cmp.test.PollReqContentTest` | HotSpot | PASS | 438 ms | PASS | 54 ms | 8.11x |
| 39 | `org.bouncycastle.asn1.cms.test.AttributeTableUnitTest` | CVM | PASS | 1,235 ms | PASS | 410 ms | 3.01x |
| 40 | `org.bouncycastle.asn1.cms.test.CCMParametersTest` | HotSpot | PASS | 651 ms | PASS | 118 ms | 5.52x |
| 41 | `org.bouncycastle.asn1.cms.test.CMSSequenceFieldOrderTest` | CVM | PASS | 745 ms | PASS | 110 ms | 6.77x |
| 42 | `org.bouncycastle.asn1.cms.test.CMSTest` | HotSpot | PASS | 551 ms | PASS | 80 ms | 6.89x |
| 43 | `org.bouncycastle.asn1.cms.test.ContentTypeSequenceSizeTest` | CVM | PASS | 479 ms | PASS | 64 ms | 7.48x |
| 44 | `org.bouncycastle.asn1.cms.test.GCMParametersTest` | HotSpot | PASS | 686 ms | PASS | 128 ms | 5.36x |
| 45 | `org.bouncycastle.asn1.cms.test.KEMRecipientInfoTest` | CVM | PASS | 731 ms | PASS | 140 ms | 5.22x |
| 46 | `org.bouncycastle.asn1.cms.test.OctetStringTest` | HotSpot | PASS | 729 ms | PASS | 136 ms | 5.36x |
| 47 | `org.bouncycastle.asn1.cms.test.ParseTest` | CVM | PASS | 734 ms | PASS | 142 ms | 5.17x |
| 48 | `org.bouncycastle.asn1.cms.test.SignerInfoTest` | HotSpot | PASS | 442 ms | PASS | 60 ms | 7.37x |
| 49 | `org.bouncycastle.asn1.crmf.test.CRMFSequenceFieldOrderTest` | CVM | PASS | 729 ms | PASS | 108 ms | 6.75x |
| 50 | `org.bouncycastle.asn1.crmf.test.DhSigStaticTest` | HotSpot | PASS | 516 ms | PASS | 72 ms | 7.17x |

*... Full table of all 1444 classes available in `bouncy_castle_interleaved_results.tsv` and `bouncy_castle_interleaved_results.json`.* 

